<script lang="ts">
  // V14 Phase F: the Preview tab's toolbar + embedded-webview host. Rendered
  // by `Pane.svelte` for `preview`-kind tabs. The pane BODY (the div below
  // the toolbar bar) is an empty measured div — the actual page content
  // lives in a native WebView2 child webview the backend positions over
  // this div's rect (`preview::preview_set_rect`); nothing is drawn into the
  // DOM here.
  //
  // Lifecycle (see `lib/preview/state.ts`'s doc comment for the full
  // rationale): this component mounts/unmounts with Svelte's `{#if}` in
  // `Pane.svelte` rather than living in a persistent per-tab registry the
  // way xterm hosts do (`terminals.ts`) — so "hide (not destroy) on
  // tab-switch away; destroy on close" is implemented by checking, ON
  // UNMOUNT, whether this tab id still exists in the `tabs` store: still
  // there ⇒ the pane just switched away (`previewHide`); gone ⇒ the tab was
  // actually closed (`previewClose`). `markPreviewOpen`/`isPreviewBackendOpen`
  // track which tab ids currently have a live (open, possibly hidden)
  // backend webview so a re-mount calls `previewShow` instead of
  // `previewOpen`.
  import { onMount } from 'svelte';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import {
    previewCapture,
    previewClose,
    previewHide,
    previewNavigate,
    previewOpen,
    previewReload,
    previewSetRect,
    previewShow,
    previewUpdateConfig,
  } from './ipc';
  import { settings } from './settings/store';
  import { tabMeta } from './tabs/store';
  import { openComposeWithAttachment } from './composeState';
  import { isPreviewBackendOpen, markPreviewClosed, markPreviewOpen } from './preview/state';
  import { navigateBack, navigateForward } from './preview/history';
  import {
    computePreviewRect,
    DEFAULT_PREVIEW_URL,
    DEVICE_PRESETS,
    isAllowedPreviewHost,
    type Rect,
  } from './preview/policy';
  import type { TabId } from './tabs/types';
  import type { PreviewTabConfig } from './settings/types';

  let { tabId }: { tabId: TabId } = $props();

  let bodyEl: HTMLDivElement | undefined = $state();

  // Seeded once from the persisted `PreviewTabConfig` on mount, then
  // entirely local/editable — NOT kept in a reactive `$effect` synced to
  // `$settings`, which would clobber in-progress typing every time our own
  // `previewUpdateConfig` echoes back through the `settings-changed`
  // broadcast (same value, but still a re-render).
  let urlInput = $state('');
  let deviceWidth = $state<number | null>(null);
  let autoReload = $state(false);
  let statusMessage = $state<string | null>(null);
  /// Simple back-stack of previously-navigated URLs (WebView2 has no wry-
  /// exposed history API this toolbar reaches for) — pushed on every
  /// successful navigate, popped by the Back button.
  let backStack: string[] = $state([]);

  function findConfig(): PreviewTabConfig | null {
    const entry = $settings.tabs.find((t) => t.id === tabId && t.kind === 'preview');
    return (entry as PreviewTabConfig | undefined) ?? null;
  }

  function currentRect(): Rect | null {
    if (!bodyEl) return null;
    const r = bodyEl.getBoundingClientRect();
    return computePreviewRect(
      { x: r.left, y: r.top, width: r.width, height: r.height },
      deviceWidth,
    );
  }

  async function applyRect(): Promise<void> {
    const rect = currentRect();
    if (!rect) return;
    try {
      await previewSetRect(tabId, rect);
    } catch (e) {
      console.error('preview_set_rect failed:', e);
    }
  }

  let unlistenFsBatch: UnlistenFn | null = null;
  let reloadDebounce: ReturnType<typeof setTimeout> | undefined;

  function wireAutoReload(): void {
    unlistenFsBatch?.();
    unlistenFsBatch = null;
    if (!autoReload) return;
    void listen('fs-batch', () => {
      if (reloadDebounce) clearTimeout(reloadDebounce);
      // ~1s quiet period following a batch, per the milestone's Phase F4 —
      // only fires while this component is mounted, which (per the
      // lifecycle above) IS "the tab is visible".
      reloadDebounce = setTimeout(() => {
        void previewReload(tabId).catch((e) => console.error('preview auto-reload failed:', e));
      }, 1000);
    }).then((fn) => {
      unlistenFsBatch = fn;
    });
  }

  onMount(() => {
    const config = findConfig();
    urlInput = config?.url || $settings.preview_last_url || DEFAULT_PREVIEW_URL;
    deviceWidth = config?.device_width ?? null;
    autoReload = config?.auto_reload ?? false;

    let ro: ResizeObserver | undefined;
    let disposed = false;

    (async () => {
      const rect = currentRect();
      if (!rect) return;
      try {
        if (isPreviewBackendOpen(tabId)) {
          await previewShow(tabId);
          await previewSetRect(tabId, rect);
        } else {
          await previewOpen(tabId, urlInput, rect);
          markPreviewOpen(tabId);
        }
      } catch (e) {
        statusMessage = String(e);
      }
      if (disposed || !bodyEl) return;
      ro = new ResizeObserver(() => void applyRect());
      ro.observe(bodyEl);
    })();

    wireAutoReload();

    return () => {
      disposed = true;
      ro?.disconnect();
      unlistenFsBatch?.();
      if (reloadDebounce) clearTimeout(reloadDebounce);

      // Still in the tabs store ⇒ the pane switched away from this tab
      // (hide, keep it alive); gone ⇒ the tab itself was closed (destroy).
      if (tabMeta(tabId)) {
        void previewHide(tabId).catch((e) => console.error('preview_hide failed:', e));
      } else {
        markPreviewClosed(tabId);
        void previewClose(tabId).catch((e) => console.error('preview_close failed:', e));
      }
    };
  });

  function persistConfig(): void {
    void previewUpdateConfig(tabId, urlInput, deviceWidth, autoReload).catch((e) => {
      console.error('preview_update_config failed:', e);
    });
  }

  // V14 code-review fix (FIX 9): `fromBack` distinguishes a Back-button
  // navigation from every other kind (URL bar submit, a rejected-navigation
  // fallback, ...). Only the latter pushes onto `backStack` — see
  // `preview/history.ts`'s doc comment for the oscillation bug this fixes.
  async function navigateTo(url: string, opts: { fromBack?: boolean } = {}): Promise<void> {
    const trimmed = url.trim();
    if (!trimmed) return;
    if (!isAllowedPreviewHost(trimmed, $settings.preview_allow_remote)) {
      statusMessage = `${trimmed} is outside the localhost/RFC-1918 policy — opened in your browser instead.`;
    } else {
      statusMessage = null;
    }
    try {
      await previewNavigate(tabId, trimmed);
      if (!opts.fromBack) {
        backStack = navigateForward({ backStack, current: urlInput }, trimmed).backStack;
      }
      urlInput = trimmed;
      persistConfig();
    } catch (e) {
      statusMessage = String(e);
    }
  }

  function onUrlSubmit(e: SubmitEvent): void {
    e.preventDefault();
    void navigateTo(urlInput);
  }

  function onBack(): void {
    const next = navigateBack({ backStack, current: urlInput });
    if (next.current === urlInput) return; // stack was empty; nothing to go back to
    backStack = next.backStack;
    void navigateTo(next.current, { fromBack: true });
  }

  function onReload(): void {
    void previewReload(tabId).catch((e) => {
      statusMessage = String(e);
    });
  }

  function onDevicePreset(width: number | null): void {
    deviceWidth = width;
    persistConfig();
    void applyRect();
  }

  function onAutoReloadToggle(): void {
    autoReload = !autoReload;
    persistConfig();
    wireAutoReload();
  }

  async function onSnapshot(): Promise<void> {
    try {
      const path = await previewCapture(tabId);
      statusMessage = null;
      openComposeWithAttachment(path);
    } catch (e) {
      statusMessage = String(e);
    }
  }
</script>

<div class="preview-pane">
  <form class="preview-toolbar" onsubmit={onUrlSubmit}>
    <button
      type="button"
      class="tb-btn"
      title="Back"
      aria-label="Back"
      disabled={backStack.length === 0}
      onclick={onBack}
    >
      ←
    </button>
    <button type="button" class="tb-btn" title="Reload" aria-label="Reload" onclick={onReload}>
      ⟳
    </button>
    <input
      class="url-input"
      type="text"
      bind:value={urlInput}
      spellcheck="false"
      aria-label="Preview URL"
      placeholder={DEFAULT_PREVIEW_URL}
    />
    <button type="submit" class="tb-btn go-btn">Go</button>
    <div class="device-presets" role="group" aria-label="Device width preset">
      {#each DEVICE_PRESETS as preset (preset.id)}
        <button
          type="button"
          class="tb-btn preset-btn"
          class:active={deviceWidth === preset.width}
          onclick={() => onDevicePreset(preset.width)}
        >
          {preset.label}
        </button>
      {/each}
    </div>
    <label class="auto-reload">
      <input type="checkbox" checked={autoReload} onchange={onAutoReloadToggle} />
      Auto-reload
    </label>
    <button type="button" class="tb-btn snapshot-btn" onclick={onSnapshot}>
      Snapshot → compose
    </button>
  </form>
  {#if statusMessage}
    <div class="preview-status">{statusMessage}</div>
  {/if}
  <div class="preview-body" bind:this={bodyEl}></div>
</div>

<style>
  .preview-pane {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }
  .preview-toolbar {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    padding: 0.375rem 0.5rem;
    background: var(--surface-1, #1a1a1a);
    border-bottom: 1px solid var(--border-subtle, #333);
    flex: 0 0 auto;
  }
  .tb-btn {
    background: transparent;
    border: 1px solid var(--border-default, #444);
    border-radius: 4px;
    color: inherit;
    padding: 0.25rem 0.5rem;
    cursor: pointer;
    font-size: 0.85rem;
  }
  .tb-btn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .tb-btn:hover:not(:disabled) {
    background: var(--surface-2, #2a2a2a);
  }
  .tb-btn.active {
    background: var(--accent, #3b82f6);
    color: var(--on-accent, #fff);
    border-color: var(--accent, #3b82f6);
  }
  .url-input {
    flex: 1 1 auto;
    min-width: 6rem;
    background: var(--surface-0, #111);
    border: 1px solid var(--border-default, #444);
    border-radius: 4px;
    color: inherit;
    padding: 0.25rem 0.5rem;
    font-size: 0.85rem;
  }
  .device-presets {
    display: flex;
    gap: 0.25rem;
  }
  .auto-reload {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    font-size: 0.8rem;
    white-space: nowrap;
  }
  .preview-status {
    flex: 0 0 auto;
    padding: 0.25rem 0.5rem;
    font-size: 0.8rem;
    background: var(--warning-bg, #402a00);
    color: var(--warning-fg, #ffb454);
  }
  .preview-body {
    position: relative;
    flex: 1 1 auto;
    min-width: 0;
    min-height: 0;
    background: var(--surface-0, #111);
  }
</style>

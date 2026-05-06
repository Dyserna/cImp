<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    initSettings,
    settings,
    applySettings,
  } from './lib/settings/store';
  import {
    aiToolTabDefaults,
    listVoices,
    requestTabRestart,
  } from './lib/settings/ipc';
  import type {
    AiToolTabConfig,
    Settings,
    ShellTabConfig,
    TabConfig,
  } from './lib/settings/types';
  import { findTab, findTabIndex } from './lib/settings/types';
  import type { AiTabId, TabId } from './lib/tabs/types';
  import { AI_TABS } from './lib/tabs/types';
  import ShortcutCapture from './lib/settings/ShortcutCapture.svelte';
  import TabSettingsSection from './lib/settings/TabSettingsSection.svelte';

  let voices = $state<string[]>([]);
  // Per-tab "applied" baselines — used to compute the Restart Required
  // indicator when subprocess-affecting fields drift from the spawn-time
  // settings. Notification text and first-launch dismissal are NOT in
  // the diff because they apply live without restart. Keyed by tab id
  // so additional AI tabs in future versions plug in without a refactor.
  let tabBaselines = $state<Record<string, AiToolTabConfig | null>>({});
  // Per-tab default settings, fetched from the backend so "Reset to default"
  // buttons match the Rust-side defaults exactly (in particular the embedded
  // RUNTIME_SYSTEM_PROMPT for Claude's TTS instructions).
  let tabDefaults = $state<Record<string, AiToolTabConfig | null>>({});
  let snapshot = $state<Settings | null>(null);

  // Keep `snapshot` in sync with the global store. Every input mutates
  // `snapshot` and pushes via `applySettings`; the broadcast comes back and
  // overwrites `snapshot` (which is fine — same value, no churn).
  let unsub: (() => void) | undefined;

  function aiTabFromSnapshot(id: string): AiToolTabConfig | null {
    if (!snapshot) return null;
    const entry = findTab(snapshot, id);
    return entry && entry.kind === 'ai_tool' ? entry : null;
  }

  function captureBaseline(tab: AiTabId) {
    const entry = aiTabFromSnapshot(tab);
    if (!entry) return;
    tabBaselines = {
      ...tabBaselines,
      [tab]: structuredClone($state.snapshot(entry)),
    };
  }

  onMount(async () => {
    await initSettings();
    snapshot = structuredClone(get(settings));
    for (const t of AI_TABS) captureBaseline(t);
    unsub = settings.subscribe((s) => {
      snapshot = structuredClone(s);
    });
    listVoices()
      .then((v) => {
        voices = v.length > 0 ? v : [snapshot?.tts.voice ?? 'af_heart'];
      })
      .catch((e) => console.warn('list_voices failed', e));
    for (const t of AI_TABS) {
      aiToolTabDefaults(t)
        .then((d) => {
          tabDefaults = { ...tabDefaults, [t]: d };
        })
        .catch((e) => console.warn(`ai_tool_tab_defaults(${t}) failed`, e));
    }
  });

  onDestroy(() => unsub?.());

  /// Mutate the live snapshot via `updater`, then push to the backend.
  /// Backend's debounced save coalesces rapid calls (slider drags).
  function patch(updater: (s: Settings) => void) {
    if (!snapshot) return;
    const next = structuredClone($state.snapshot(snapshot));
    updater(next);
    snapshot = next;
    void applySettings(next);
  }

  /// Replace the AI-tab entry at `id` in the snapshot. Used by the
  /// TabSettingsSection's bound setter; the array shape forces the
  /// find-by-id lookup at write time.
  function patchAiTab(id: string, value: AiToolTabConfig) {
    patch((s) => {
      const idx = findTabIndex(s, id);
      if (idx < 0) return;
      s.tabs[idx] = value;
    });
  }

  // Restart-affecting subset: command + args + cwd + env + TTS injection.
  // Notifications and first_launch_notice_dismissed apply live and are
  // excluded.
  function restartShape(t: AiToolTabConfig) {
    return {
      command: t.command,
      args: t.args,
      cwd: t.cwd,
      env: t.env,
      tts_injection: t.tts_injection,
    };
  }

  const restartRequired = $derived.by(() => {
    const out: Record<string, boolean> = {};
    if (!snapshot) return out;
    for (const t of AI_TABS) {
      const baseline = tabBaselines[t];
      const live = aiTabFromSnapshot(t);
      if (!baseline || !live) continue;
      out[t] = JSON.stringify(restartShape(live)) !== JSON.stringify(restartShape(baseline));
    }
    return out;
  });

  async function restartTab(tab: AiTabId) {
    await requestTabRestart(tab);
    captureBaseline(tab);
  }

  /// Tabs visible in the Tabs section, in their stored order. Filtered
  /// view of `snapshot.tabs` so the template can render AI tabs and Shell
  /// tabs differently. Empty array when settings haven't loaded yet.
  const tabEntries = $derived<TabConfig[]>(snapshot?.tabs ?? []);

  function aiTabAt(id: string): AiToolTabConfig | null {
    return aiTabFromSnapshot(id);
  }

  function shellSummary(t: ShellTabConfig): string {
    const args = t.args.length > 0 ? ' ' + t.args.join(' ') : '';
    return `${t.command}${args}`;
  }

  /// Open the main window and emit a "configure tab" request that the tab
  /// bar's right-click menu component listens for. Keeps the inline-editor
  /// concern out of the settings window — Shell tab editing is one path
  /// (the Configure dialog), invoked from either the tab bar context menu
  /// or this settings list.
  async function configureShellTab(_tabId: TabId) {
    // Stub for now — the Configure dialog wiring is owned by the main
    // window's tab-bar code. A future polish PR can route through a
    // dedicated event; for v1.2 the user is told to right-click the tab.
  }

  async function pickFile(
    name: string,
    extensions: string[],
  ): Promise<string | null> {
    try {
      const r = await open({ multiple: false, filters: [{ name, extensions }] });
      if (typeof r === 'string') return r;
      return null;
    } catch (e) {
      console.error('dialog open failed', e);
      return null;
    }
  }

  function imagePicker(state: keyof Settings['avatar']['images']) {
    return async () => {
      const p = await pickFile('Image / Video', [
        'png',
        'jpg',
        'jpeg',
        'gif',
        'webp',
        'mp4',
        'webm',
        'mov',
      ]);
      if (p === null) return;
      patch((s) => {
        s.avatar.images[state] = p;
      });
    };
  }

  async function pickTransition() {
    const p = await pickFile('Image / Video', [
      'png',
      'jpg',
      'jpeg',
      'gif',
      'webp',
      'mp4',
      'webm',
      'mov',
    ]);
    if (p === null) return;
    patch((s) => {
      s.avatar.transition.path = p;
    });
  }

  function basename(p: string | null): string {
    if (!p) return '— not set —';
    return p.split(/[/\\]/).pop() ?? p;
  }
</script>

{#if !snapshot}
  <div class="loading">Loading settings…</div>
{:else}
  <div class="root">
    <div class="inner">
    <header>
      <h1>Settings</h1>
    </header>

    <section>
      <h2>TTS</h2>
      <label>
        <span>Voice</span>
        <select
          value={snapshot.tts.voice}
          onchange={(e) => patch((s) => (s.tts.voice = (e.currentTarget as HTMLSelectElement).value))}
        >
          {#each voices as v}
            <option value={v}>{v}</option>
          {/each}
        </select>
      </label>
      <label>
        <span>Speed: {snapshot.tts.speed.toFixed(2)}×</span>
        <input
          type="range"
          min="0.5"
          max="2"
          step="0.05"
          value={snapshot.tts.speed}
          oninput={(e) =>
            patch((s) => (s.tts.speed = +(e.currentTarget as HTMLInputElement).value))}
        />
      </label>
      <label>
        <span>Volume: {Math.round(snapshot.tts.volume * 100)}%</span>
        <input
          type="range"
          min="0"
          max="1"
          step="0.01"
          value={snapshot.tts.volume}
          oninput={(e) =>
            patch((s) => (s.tts.volume = +(e.currentTarget as HTMLInputElement).value))}
        />
      </label>
      <label class="checkbox">
        <input
          type="checkbox"
          checked={snapshot.tts.mute}
          onchange={(e) =>
            patch((s) => (s.tts.mute = (e.currentTarget as HTMLInputElement).checked))}
        />
        <span>Mute</span>
      </label>
    </section>

    <section>
      <h2>Avatar</h2>
      <label class="checkbox">
        <input
          type="checkbox"
          checked={snapshot.avatar.visible}
          onchange={(e) =>
            patch((s) => (s.avatar.visible = (e.currentTarget as HTMLInputElement).checked))}
        />
        <span>Visible</span>
      </label>
      <label>
        <span>Position</span>
        <select
          value={snapshot.avatar.position}
          onchange={(e) =>
            patch((s) => (s.avatar.position = (e.currentTarget as HTMLSelectElement).value as Settings['avatar']['position']))}
        >
          <option value="top-right">Top Right</option>
          <option value="top-left">Top Left</option>
          <option value="bottom-right">Bottom Right</option>
          <option value="bottom-left">Bottom Left</option>
        </select>
      </label>
      <div class="row">
        <label>
          <span>Width (px)</span>
          <input
            type="number"
            min="50"
            max="1200"
            value={snapshot.avatar.size.width_px}
            onchange={(e) =>
              patch((s) => (s.avatar.size.width_px = Math.max(50, +(e.currentTarget as HTMLInputElement).value)))}
          />
        </label>
        <label>
          <span>Height (px)</span>
          <input
            type="number"
            min="50"
            max="1200"
            value={snapshot.avatar.size.height_px}
            onchange={(e) =>
              patch((s) => (s.avatar.size.height_px = Math.max(50, +(e.currentTarget as HTMLInputElement).value)))}
          />
        </label>
      </div>
      <label>
        <span>Margin (px)</span>
        <input
          type="number"
          min="0"
          max="200"
          value={snapshot.avatar.margin_px}
          onchange={(e) =>
            patch((s) => (s.avatar.margin_px = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
        />
      </label>
      <label>
        <span>Opacity: {Math.round(snapshot.avatar.opacity * 100)}%</span>
        <input
          type="range"
          min="0.3"
          max="1"
          step="0.01"
          value={snapshot.avatar.opacity}
          oninput={(e) =>
            patch((s) => (s.avatar.opacity = +(e.currentTarget as HTMLInputElement).value))}
        />
      </label>

      <h3>Per-state images</h3>
      {#each ['idle', 'listening', 'thinking', 'speaking', 'error'] as const as state}
        <div class="file-row">
          <span class="state-label">{state}</span>
          <span class="filename" title={snapshot.avatar.images[state] ?? ''}>
            {basename(snapshot.avatar.images[state])}
          </span>
          <button onclick={imagePicker(state)}>Pick…</button>
          <button
            class="ghost"
            onclick={() => patch((s) => (s.avatar.images[state] = null))}
            disabled={snapshot.avatar.images[state] === null}
          >
            Reset
          </button>
        </div>
      {/each}

      <h3>Transition</h3>
      <div class="file-row">
        <span class="state-label">Path</span>
        <span class="filename" title={snapshot.avatar.transition.path ?? ''}>
          {basename(snapshot.avatar.transition.path)}
        </span>
        <button onclick={pickTransition}>Pick…</button>
        <button
          class="ghost"
          onclick={() => patch((s) => (s.avatar.transition.path = null))}
          disabled={snapshot.avatar.transition.path === null}
        >
          Clear
        </button>
      </div>
      <small class="hint">An empty path disables transitions (states snap directly).</small>
      <label>
        <span>Duration (ms)</span>
        <input
          type="number"
          min="0"
          max="5000"
          step="50"
          value={snapshot.avatar.transition.duration_ms}
          onchange={(e) =>
            patch((s) => (s.avatar.transition.duration_ms = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
        />
      </label>
    </section>

    <section>
      <h2>Waveform</h2>
      <label>
        <span>Color</span>
        <input
          type="color"
          value={snapshot.avatar.waveform.color}
          oninput={(e) =>
            patch((s) => (s.avatar.waveform.color = (e.currentTarget as HTMLInputElement).value))}
        />
      </label>
      <label>
        <span>Line width: {snapshot.avatar.waveform.line_width.toFixed(1)}</span>
        <input
          type="range"
          min="0.5"
          max="8"
          step="0.5"
          value={snapshot.avatar.waveform.line_width}
          oninput={(e) =>
            patch((s) => (s.avatar.waveform.line_width = +(e.currentTarget as HTMLInputElement).value))}
        />
      </label>
      <label>
        <span>Glow: {Math.round(snapshot.avatar.waveform.glow_intensity * 100)}%</span>
        <input
          type="range"
          min="0"
          max="1"
          step="0.05"
          value={snapshot.avatar.waveform.glow_intensity}
          oninput={(e) =>
            patch((s) => (s.avatar.waveform.glow_intensity = +(e.currentTarget as HTMLInputElement).value))}
        />
      </label>
      <label>
        <span>Opacity: {Math.round(snapshot.avatar.waveform.opacity * 100)}%</span>
        <input
          type="range"
          min="0"
          max="1"
          step="0.01"
          value={snapshot.avatar.waveform.opacity}
          oninput={(e) =>
            patch((s) => (s.avatar.waveform.opacity = +(e.currentTarget as HTMLInputElement).value))}
        />
      </label>
    </section>

    <section>
      <h2>Display</h2>
      <label>
        <span>Terminal font family</span>
        <input
          type="text"
          value={snapshot.display.terminal_font_family}
          onchange={(e) =>
            patch((s) => (s.display.terminal_font_family = (e.currentTarget as HTMLInputElement).value))}
        />
      </label>
      <label>
        <span>Terminal font size (px)</span>
        <input
          type="number"
          min="8"
          max="48"
          value={snapshot.display.terminal_font_size}
          onchange={(e) =>
            patch((s) => (s.display.terminal_font_size = Math.max(8, +(e.currentTarget as HTMLInputElement).value)))}
        />
      </label>
      <label>
        <span>Theme</span>
        <select
          value={snapshot.display.theme}
          onchange={(e) =>
            patch((s) => (s.display.theme = (e.currentTarget as HTMLSelectElement).value))}
        >
          <option value="dark">Dark</option>
          <option value="light">Light</option>
        </select>
      </label>
      <label class="checkbox">
        <input
          type="checkbox"
          checked={snapshot.display.show_tts_markup}
          onchange={(e) =>
            patch((s) => (s.display.show_tts_markup = (e.currentTarget as HTMLInputElement).checked))}
        />
        <span>Show TTS markup in terminal (debug)</span>
      </label>
    </section>

    <section>
      <h2>Behavior</h2>
      <label class="checkbox">
        <input
          type="checkbox"
          checked={snapshot.behavior.interrupt_on_input}
          onchange={(e) =>
            patch((s) => (s.behavior.interrupt_on_input = (e.currentTarget as HTMLInputElement).checked))}
        />
        <span>Interrupt TTS when typing</span>
      </label>
      <label class="checkbox">
        <input
          type="checkbox"
          checked={snapshot.behavior.auto_speak}
          onchange={(e) =>
            patch((s) => (s.behavior.auto_speak = (e.currentTarget as HTMLInputElement).checked))}
        />
        <span>Auto-speak detected segments</span>
      </label>
      <label class="checkbox disabled">
        <input type="checkbox" checked={snapshot.behavior.fallback_silent} disabled />
        <span>Fallback silent on TTS error (always on in v1)</span>
      </label>
    </section>

    <section>
      <h2>Compose</h2>
      <div class="row">
        <label>
          <span>Min height (px)</span>
          <input
            type="number"
            min="40"
            max="400"
            value={snapshot.compose.min_height_px}
            onchange={(e) =>
              patch((s) => (s.compose.min_height_px = Math.max(40, +(e.currentTarget as HTMLInputElement).value)))}
          />
        </label>
        <label>
          <span>Max height (px)</span>
          <input
            type="number"
            min="60"
            max="800"
            value={snapshot.compose.max_height_px}
            onchange={(e) =>
              patch((s) => (s.compose.max_height_px = Math.max(60, +(e.currentTarget as HTMLInputElement).value)))}
          />
        </label>
      </div>
    </section>

    <section>
      <h2>Shortcuts</h2>
      <label>
        <span>Open compose</span>
        <ShortcutCapture
          bind:value={
            () => snapshot!.shortcuts.open_compose,
            (v) => patch((s) => (s.shortcuts.open_compose = v))
          }
        />
      </label>
      <label>
        <span>Submit compose</span>
        <ShortcutCapture
          bind:value={
            () => snapshot!.shortcuts.submit_compose,
            (v) => patch((s) => (s.shortcuts.submit_compose = v))
          }
        />
      </label>
      <label>
        <span>Cancel compose</span>
        <ShortcutCapture
          bind:value={
            () => snapshot!.shortcuts.cancel_compose,
            (v) => patch((s) => (s.shortcuts.cancel_compose = v))
          }
        />
      </label>
      <label>
        <span>Open settings</span>
        <ShortcutCapture
          bind:value={
            () => snapshot!.shortcuts.open_settings,
            (v) => patch((s) => (s.shortcuts.open_settings = v))
          }
        />
      </label>
      <label>
        <span>Switch to Claude tab</span>
        <ShortcutCapture
          bind:value={
            () => snapshot!.shortcuts.switch_to_tab_1,
            (v) => patch((s) => (s.shortcuts.switch_to_tab_1 = v))
          }
        />
      </label>
      <label>
        <span>Switch to Aider tab</span>
        <ShortcutCapture
          bind:value={
            () => snapshot!.shortcuts.switch_to_tab_2,
            (v) => patch((s) => (s.shortcuts.switch_to_tab_2 = v))
          }
        />
      </label>
    </section>

    <section>
      <h2>Tabs</h2>
      <small class="hint">
        All configured tabs in their stored order. AI builtins expand inline;
        Shell tabs show a summary — edit them via right-click → Configure on
        the tab bar.
      </small>
      <div class="tabs-grid">
        {#each tabEntries as entry (entry.id)}
          {#if entry.kind === 'ai_tool'}
            {@const live = aiTabAt(entry.id)}
            <details open>
              <summary>
                {entry.name}
                <span class="kind-badge ai">AI</span>
              </summary>
              {#if live}
                <TabSettingsSection
                  tabId={entry.id as TabId}
                  displayName={entry.name}
                  bind:settings={
                    () => live,
                    (v) => patchAiTab(entry.id, v)
                  }
                  defaults={tabDefaults[entry.id] ?? null}
                  restartRequired={restartRequired[entry.id] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab(entry.id as AiTabId)}
                />
              {/if}
            </details>
          {:else}
            <div class="shell-row">
              <div class="shell-row-head">
                <span class="shell-name">{entry.name}</span>
                <span class="kind-badge shell">Shell</span>
                {#if entry.builtin}
                  <span class="builtin-tag">builtin</span>
                {/if}
              </div>
              <div class="shell-row-cmd" title={shellSummary(entry)}>
                {shellSummary(entry)}
              </div>
              <div class="shell-row-actions">
                <button
                  type="button"
                  class="ghost"
                  onclick={() => configureShellTab(entry.id as TabId)}
                >
                  Configure…
                </button>
              </div>
            </div>
          {/if}
        {/each}
      </div>
    </section>

    <section>
      <h2>Processing</h2>
      <div class="row">
        <label>
          <span>Stability timeout (ms)</span>
          <input
            type="number"
            min="0"
            max="2000"
            step="10"
            value={snapshot.processing.stability_timeout_ms}
            onchange={(e) =>
              patch((s) => (s.processing.stability_timeout_ms = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
          />
        </label>
        <label>
          <span>Max hold (ms)</span>
          <input
            type="number"
            min="50"
            max="5000"
            step="50"
            value={snapshot.processing.max_hold_ms}
            onchange={(e) =>
              patch((s) => (s.processing.max_hold_ms = Math.max(50, +(e.currentTarget as HTMLInputElement).value)))}
          />
        </label>
      </div>
    </section>
    </div>
  </div>
{/if}

<style>
  :global(html, body) {
    background: #1a1a1a;
    color: #ddd;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 13px;
  }
  /* The settings page lives inside #app, which app.css pins to the
     viewport. Rather than fight the shared global with overrides (whose
     load-order winning isn't guaranteed across HMR/build), make .root
     the scroll container and size it to fill #app. The sticky header
     stays pinned to the top of this container as it scrolls. */
  .root {
    height: 100vh;
    overflow-y: auto;
    overflow-x: hidden;
    padding: 16px 20px 32px;
    box-sizing: border-box;
  }
  .inner {
    max-width: 720px;
    margin: 0 auto;
  }
  .loading {
    padding: 32px;
    text-align: center;
    color: #888;
  }
  header {
    position: sticky;
    top: 0;
    background: #1a1a1a;
    border-bottom: 1px solid #333;
    padding-bottom: 8px;
    margin-bottom: 16px;
    z-index: 1;
  }
  h1 {
    margin: 0;
    font-size: 18px;
    font-weight: 600;
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
    margin: 0 0 12px 0;
    color: #bb55ff;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  h3 {
    font-size: 12px;
    font-weight: 600;
    margin: 16px 0 6px 0;
    color: #aaa;
  }
  section {
    border: 1px solid #2a2a2a;
    border-radius: 6px;
    padding: 16px;
    margin-bottom: 16px;
    background: #1f1f1f;
  }
  label {
    display: block;
    margin-bottom: 12px;
  }
  label > span:first-child {
    display: block;
    margin-bottom: 4px;
    color: #aaa;
    font-size: 12px;
  }
  label.checkbox {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  label.checkbox > span {
    margin: 0;
  }
  label.checkbox.disabled {
    opacity: 0.6;
  }
  input[type='text'],
  input[type='number'],
  select {
    width: 100%;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    padding: 6px 8px;
    border-radius: 4px;
    font-family: inherit;
    font-size: 13px;
    box-sizing: border-box;
  }
  input[type='range'] {
    width: 100%;
  }
  input[type='color'] {
    height: 32px;
    padding: 0;
    border: 1px solid #444;
    background: #2a2a2a;
    border-radius: 4px;
  }
  .row {
    display: flex;
    gap: 12px;
  }
  .row > label {
    flex: 1;
  }
  .file-row {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-bottom: 6px;
  }
  .state-label {
    width: 80px;
    color: #aaa;
    font-size: 12px;
    text-transform: capitalize;
  }
  .filename {
    flex: 1;
    color: #ddd;
    font-family: monospace;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button {
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    padding: 6px 12px;
    border-radius: 4px;
    font-size: 12px;
    cursor: pointer;
  }
  button:hover:not(:disabled) {
    background: #333;
    border-color: #555;
  }
  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  button.ghost {
    background: transparent;
  }
  small.hint {
    display: block;
    color: #888;
    font-size: 11px;
    margin: -8px 0 12px 0;
  }
  .tabs-grid {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 8px;
  }
  details {
    border: 1px solid #2a2a2a;
    border-radius: 6px;
    background: #181818;
  }
  details[open] {
    background: #1a1a1a;
  }
  summary {
    cursor: pointer;
    padding: 8px 12px;
    color: #ddd;
    font-weight: 600;
    font-size: 12px;
    user-select: none;
    border-radius: 6px;
  }
  summary:hover {
    background: #222;
  }
  details[open] > summary {
    border-bottom: 1px solid #2a2a2a;
    border-radius: 6px 6px 0 0;
  }
  .kind-badge {
    display: inline-block;
    font-size: 9px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 1px 6px;
    border-radius: 8px;
    margin-left: 6px;
    vertical-align: middle;
    font-weight: 600;
  }
  .kind-badge.ai {
    background: #2a1f3a;
    border: 1px solid #6f42a8;
    color: #d8b8ff;
  }
  .kind-badge.shell {
    background: #1a2a1a;
    border: 1px solid #4a8a4a;
    color: #b8e0b8;
  }
  .builtin-tag {
    display: inline-block;
    font-size: 9px;
    text-transform: uppercase;
    color: #888;
    border: 1px solid #444;
    padding: 1px 6px;
    border-radius: 8px;
    margin-left: 6px;
    vertical-align: middle;
  }
  .shell-row {
    border: 1px solid #2a2a2a;
    border-radius: 6px;
    background: #181818;
    padding: 10px 12px;
    display: grid;
    grid-template-columns: 1fr auto;
    grid-template-rows: auto auto;
    gap: 4px 10px;
    align-items: center;
  }
  .shell-row-head {
    grid-column: 1;
    grid-row: 1;
    color: #ddd;
    font-size: 12px;
    font-weight: 600;
  }
  .shell-row-cmd {
    grid-column: 1;
    grid-row: 2;
    font-family: monospace;
    font-size: 11px;
    color: #888;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .shell-row-actions {
    grid-column: 2;
    grid-row: 1 / span 2;
  }
  .shell-row-actions button {
    background: #2a2a2a;
    border: 1px solid #444;
    color: #aaa;
    padding: 4px 12px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
  }
  .shell-row-actions button:hover {
    background: #333;
    color: #ddd;
  }
  .shell-name {
    margin-right: 4px;
  }
</style>

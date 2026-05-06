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
    listVoices,
    requestTabRestart,
    tabDefaultSettings,
  } from './lib/settings/ipc';
  import type { Settings, TabSettings } from './lib/settings/types';
  import type { AiTabId } from './lib/tabs/types';
  import { AI_TABS, TAB_META } from './lib/tabs/types';

  // Shell-tab settings live under `_shell_1_tmp` and don't share the
  // TabSettings shape, so M1's Settings window still scopes to AI tabs
  // only. M4 of v3-01 adds Shell-tab settings rows.
  const AI_TAB_META = TAB_META.filter(
    (m): m is { id: AiTabId; label: string } => m.id === 'claude' || m.id === 'aider',
  );
  import ShortcutCapture from './lib/settings/ShortcutCapture.svelte';
  import TabSettingsSection from './lib/settings/TabSettingsSection.svelte';

  let voices = $state<string[]>([]);
  // Per-tab "applied" baselines — used to compute the Restart Required
  // indicator when subprocess-affecting fields drift from the spawn-time
  // settings. Notification text and first-launch dismissal are NOT in
  // the diff because they apply live without restart.
  let tabBaselines = $state<Record<AiTabId, TabSettings | null>>({
    claude: null,
    aider: null,
  });
  // Per-tab default settings, fetched from the backend so "Reset to default"
  // buttons match the Rust-side defaults exactly (in particular the embedded
  // RUNTIME_SYSTEM_PROMPT for Claude's TTS instructions).
  let tabDefaults = $state<Record<AiTabId, TabSettings | null>>({
    claude: null,
    aider: null,
  });
  let snapshot = $state<Settings | null>(null);

  // Keep `snapshot` in sync with the global store. Every input mutates
  // `snapshot` and pushes via `applySettings`; the broadcast comes back and
  // overwrites `snapshot` (which is fine — same value, no churn).
  let unsub: (() => void) | undefined;

  function captureBaseline(tab: AiTabId) {
    if (!snapshot) return;
    tabBaselines = {
      ...tabBaselines,
      [tab]: structuredClone($state.snapshot(snapshot.tabs[tab])),
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
      tabDefaultSettings(t)
        .then((d) => {
          tabDefaults = { ...tabDefaults, [t]: d };
        })
        .catch((e) => console.warn(`tab_default_settings(${t}) failed`, e));
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

  // Restart-affecting subset: command + flags + TTS injection. Notifications
  // and first_launch_notice_dismissed apply live and are excluded.
  function restartShape(t: TabSettings) {
    return {
      command: t.command,
      extra_cli_flags: t.extra_cli_flags,
      tts_injection: t.tts_injection,
    };
  }

  const restartRequired = $derived.by(() => {
    const out: Record<AiTabId, boolean> = { claude: false, aider: false };
    if (!snapshot) return out;
    for (const t of AI_TABS) {
      const baseline = tabBaselines[t];
      if (!baseline) continue;
      out[t] =
        JSON.stringify(restartShape(snapshot.tabs[t])) !==
        JSON.stringify(restartShape(baseline));
    }
    return out;
  });

  async function restartTab(tab: AiTabId) {
    await requestTabRestart(tab);
    captureBaseline(tab);
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
        Per-tab subprocess configuration. Changes to command, CLI flags, or
        TTS injection require a restart of the affected tab to take effect.
      </small>
      <div class="tabs-grid">
        {#each AI_TAB_META as meta (meta.id)}
          <details open>
            <summary>{meta.label}</summary>
            <TabSettingsSection
              tabId={meta.id}
              displayName={meta.label}
              bind:settings={
                () => snapshot!.tabs[meta.id],
                (v) => patch((s) => (s.tabs[meta.id] = v))
              }
              defaults={tabDefaults[meta.id]}
              restartRequired={restartRequired[meta.id]}
              onchange={() => {}}
              onrestart={() => restartTab(meta.id)}
            />
          </details>
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
</style>

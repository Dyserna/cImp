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
  import ThemeSwatch from './lib/settings/ThemeSwatch.svelte';
  import CustomThemeEditor from './lib/settings/CustomThemeEditor.svelte';
  import { BUNDLED_THEME_NAMES, BUNDLED_THEMES, resolveBundledTheme } from './lib/themes';
  import type { ThemeColorsWire } from './lib/settings/types';

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

  // V1.4-02 terminal background mode. Derived from the (image, color)
  // pair on every snapshot change — but kept as a writable so clicking
  // "Image" before a file is picked still surfaces the image controls.
  // The mode is intent; the snapshot is the persisted reality.
  let bgMode = $state<'theme' | 'color' | 'image'>('theme');
  $effect(() => {
    if (!snapshot) return;
    const bg = snapshot.terminal.background;
    bgMode = bg.image ? 'image' : bg.color ? 'color' : 'theme';
  });

  function setBgMode(next: 'theme' | 'color' | 'image') {
    bgMode = next;
    patch((s) => {
      const bg = s.terminal.background;
      if (next === 'theme') {
        bg.image = null;
        bg.color = null;
      } else if (next === 'color') {
        bg.image = null;
        if (!bg.color) bg.color = '#1a1a1a';
      }
      // 'image': don't write yet — user picks file next via pickBgImage.
    });
  }

  async function pickBgImage() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        {
          name: 'Images',
          extensions: ['png', 'jpg', 'jpeg', 'webp', 'gif', 'bmp'],
        },
      ],
    });
    if (typeof selected === 'string') {
      patch((s) => {
        s.terminal.background.image = selected;
      });
    }
  }

  function clearBgImage() {
    patch((s) => {
      s.terminal.background.image = null;
    });
  }

  function clearBgTint() {
    patch((s) => {
      s.terminal.background.color = null;
    });
  }

  function aiTabAt(id: string): AiToolTabConfig | null {
    return aiTabFromSnapshot(id);
  }

  function shellSummary(t: ShellTabConfig): string {
    const args = t.args.length > 0 ? ' ' + t.args.join(' ') : '';
    return `${t.command}${args}`;
  }

  /// Replace the Shell-tab entry's notification config in the snapshot.
  /// Inline-editable in the Settings window (M4) — notifications apply
  /// live, no restart needed, so the existing settings broadcast flow is
  /// all we need. Spawn-affecting fields (command/args/cwd) are read-only
  /// here; the user changes them via the tab bar's right-click → Configure.
  function patchShellNotifications(
    id: string,
    next: ShellTabConfig['notifications'],
  ) {
    patch((s) => {
      const idx = findTabIndex(s, id);
      if (idx < 0) return;
      const entry = s.tabs[idx];
      if (entry.kind !== 'shell') return;
      s.tabs[idx] = { ...entry, notifications: next };
    });
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
      <h2>Appearance</h2>
      <label>
        <span>UI theme</span>
        <select
          value={snapshot.ui.theme}
          onchange={(e) =>
            patch((s) => (s.ui.theme = (e.currentTarget as HTMLSelectElement).value))}
        >
          <option value="modern-dark">Modern Dark</option>
        </select>
      </label>
      <small class="hint">
        Governs the cctts chrome — tab bar, status bar, dialogs. Distinct
        from the terminal palette below.
      </small>

      <label class="palette-row">
        <span>Terminal palette</span>
        <select
          value={snapshot.terminal.theme.name}
          onchange={(e) => {
            const name = (e.currentTarget as HTMLSelectElement).value;
            patch((s) => {
              // Read the previous name from `s` itself — `patch`'s
              // working copy holds the pre-update value at entry, which
              // is what we want for seeding.
              const previousName = s.terminal.theme.name;
              s.terminal.theme.name = name;
              if (name === 'Custom') {
                // Seed custom from the previously-active palette so the
                // user opens the editor with sensible starting colors
                // rather than 22 black squares. The seed is a snapshot;
                // edits afterwards diverge naturally.
                if (!s.terminal.theme.custom) {
                  const seed =
                    previousName === 'Custom'
                      ? BUNDLED_THEMES.Default
                      : resolveBundledTheme(previousName);
                  s.terminal.theme.custom = { ...seed } as ThemeColorsWire;
                }
              } else {
                // Drop any custom block when leaving Custom — avoids a
                // stale custom payload sitting in settings.json.
                s.terminal.theme.custom = null;
              }
            });
          }}
        >
          {#each BUNDLED_THEME_NAMES as name}
            <option value={name}>{name}</option>
          {/each}
          <option value="Custom">Custom…</option>
        </select>
        <ThemeSwatch
          name={snapshot.terminal.theme.name}
          custom={snapshot.terminal.theme.custom}
        />
      </label>
      <small class="hint">
        Colors used inside terminal tabs. Each tab can override this in
        its Configure dialog.
      </small>

      {#if snapshot.terminal.theme.name === 'Custom' && snapshot.terminal.theme.custom}
        <CustomThemeEditor
          value={snapshot.terminal.theme.custom}
          onchange={(next) =>
            patch((s) => {
              s.terminal.theme.custom = next;
            })}
        />
      {/if}

      <label class="palette-row">
        <span>Terminal background</span>
        <select
          value={bgMode}
          onchange={(e) =>
            setBgMode(
              (e.currentTarget as HTMLSelectElement).value as
                | 'theme'
                | 'color'
                | 'image',
            )}
        >
          <option value="theme">Theme default</option>
          <option value="color">Solid color</option>
          <option value="image">Image</option>
        </select>
      </label>
      <small class="hint">
        Solid color is rendered with no performance cost. Image switches
        to a slower DOM renderer (2-5× slower for high-throughput output)
        and resets the tab's scrollback when toggled.
      </small>

      {#if bgMode === 'color'}
        <label>
          <span>Background color</span>
          <input
            type="color"
            value={snapshot.terminal.background.color ?? '#1a1a1a'}
            onchange={(e) =>
              patch(
                (s) =>
                  (s.terminal.background.color = (
                    e.currentTarget as HTMLInputElement
                  ).value),
              )}
          />
        </label>
        <small class="hint">Overrides the theme's background color.</small>
      {:else if bgMode === 'image'}
        <label>
          <span>Image file</span>
          <span class="bg-image-row">
            <button type="button" onclick={pickBgImage}>Choose…</button>
            {#if snapshot.terminal.background.image}
              <code class="bg-image-path"
                >{snapshot.terminal.background.image}</code
              >
              <button type="button" onclick={clearBgImage}>Clear</button>
            {:else}
              <small class="hint">No image selected.</small>
            {/if}
          </span>
        </label>
        <label>
          <span>Opacity</span>
          <input
            type="range"
            min="0"
            max="1"
            step="0.05"
            value={snapshot.terminal.background.opacity}
            oninput={(e) =>
              patch(
                (s) =>
                  (s.terminal.background.opacity = +(
                    e.currentTarget as HTMLInputElement
                  ).value),
              )}
          />
          <small>{snapshot.terminal.background.opacity.toFixed(2)}</small>
        </label>
        <label>
          <span>Blur (px)</span>
          <input
            type="range"
            min="0"
            max="40"
            step="1"
            value={snapshot.terminal.background.blur}
            oninput={(e) =>
              patch(
                (s) =>
                  (s.terminal.background.blur = +(
                    e.currentTarget as HTMLInputElement
                  ).value),
              )}
          />
          <small>{snapshot.terminal.background.blur}</small>
        </label>
        <label>
          <span>Size</span>
          <select
            value={snapshot.terminal.background.size}
            onchange={(e) =>
              patch(
                (s) =>
                  (s.terminal.background.size = (
                    e.currentTarget as HTMLSelectElement
                  ).value as 'cover' | 'contain' | 'tile'),
              )}
          >
            <option value="cover">cover</option>
            <option value="contain">contain</option>
            <option value="tile">tile</option>
          </select>
        </label>
        <label>
          <span>Position</span>
          <input
            type="text"
            placeholder="center"
            value={snapshot.terminal.background.position}
            onchange={(e) =>
              patch(
                (s) =>
                  (s.terminal.background.position = (
                    e.currentTarget as HTMLInputElement
                  ).value),
              )}
          />
        </label>
        <label>
          <span>Tint color</span>
          <span class="bg-image-row">
            <input
              type="color"
              value={snapshot.terminal.background.color ?? '#000000'}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.terminal.background.color = (
                      e.currentTarget as HTMLInputElement
                    ).value),
                )}
            />
            {#if snapshot.terminal.background.color}
              <button type="button" onclick={clearBgTint}>Reset</button>
            {/if}
          </span>
        </label>
        <small class="hint">
          Tints the dimming overlay drawn beneath the cells. Defaults to
          black when unset.
        </small>
      {/if}
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
            <details>
              <summary>
                {entry.name}
                <span class="kind-badge shell">Shell</span>
                {#if entry.builtin}
                  <span class="builtin-tag">builtin</span>
                {/if}
              </summary>
              <div class="shell-edit">
                <label>
                  <span>Command</span>
                  <input type="text" value={shellSummary(entry)} disabled readonly />
                  <small class="hint">
                    To change the command, args, or working directory,
                    right-click the tab in the tab bar and choose
                    Configure…
                  </small>
                </label>
                <label>
                  <span>Error notification text</span>
                  <input
                    type="text"
                    value={entry.notifications.error}
                    oninput={(e) =>
                      patchShellNotifications(entry.id, {
                        ...entry.notifications,
                        error: (e.currentTarget as HTMLInputElement).value,
                      })}
                  />
                  <small class="hint">
                    Spoken when this tab errors while you're on a different
                    tab. Leave blank to disable.
                  </small>
                </label>
                <label>
                  <span>Exited notification text</span>
                  <input
                    type="text"
                    value={entry.notifications.exited}
                    oninput={(e) =>
                      patchShellNotifications(entry.id, {
                        ...entry.notifications,
                        exited: (e.currentTarget as HTMLInputElement).value,
                      })}
                  />
                  <small class="hint">
                    Spoken when this shell exits while you're on a different
                    tab. Use <code>{'{code}'}</code> to insert the exit code.
                    Leave blank to disable.
                  </small>
                </label>
              </div>
            </details>
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
    background: var(--surface-sunken);
    color: var(--text-primary);
    font-family: system-ui, -apple-system, sans-serif;
    font-size: var(--font-size-md);
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
    padding: var(--space-6);
    text-align: center;
    color: var(--text-tertiary);
  }
  header {
    position: sticky;
    top: 0;
    background: var(--surface-sunken);
    border-bottom: 1px solid var(--border-faint);
    padding-bottom: var(--space-2);
    margin-bottom: var(--space-4);
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
    margin: 0 0 var(--space-3) 0;
    color: var(--accent-purple);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  h3 {
    font-size: var(--font-size-sm);
    font-weight: 600;
    margin: var(--space-4) 0 6px 0;
    color: var(--text-quiet-strong);
  }
  section {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    padding: var(--space-4);
    margin-bottom: var(--space-4);
    background: var(--surface-1);
  }
  label {
    display: block;
    margin-bottom: var(--space-3);
  }
  label > span:first-child {
    display: block;
    margin-bottom: var(--space-1);
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    /* Tabular numerics so slider value labels (e.g. "Speed: 1.20×")
       don't jitter the label width as the value changes. */
    font-variant-numeric: tabular-nums;
    font-feature-settings: "tnum";
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
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: inherit;
    font-size: var(--font-size-md);
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  input[type='text']:focus,
  input[type='number']:focus,
  select:focus {
    outline: none;
    border-color: var(--accent);
  }
  input[type='range'] {
    width: 100%;
    accent-color: var(--accent);
  }
  input[type='color'] {
    height: 32px;
    padding: 0;
    border: 1px solid var(--border-default);
    background: var(--surface-2);
    border-radius: var(--radius-md);
  }
  .row {
    display: flex;
    gap: var(--space-3);
  }
  .row > label {
    flex: 1;
  }
  .file-row {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-bottom: 6px;
  }
  .state-label {
    width: 80px;
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    text-transform: capitalize;
  }
  .filename {
    flex: 1;
    color: var(--text-primary);
    font-family: monospace;
    font-size: var(--font-size-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button {
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-3);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  button:hover:not(:disabled) {
    background: var(--surface-input);
    border-color: var(--border-strong);
  }
  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
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
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    margin: -8px 0 var(--space-3) 0;
  }
  .tabs-grid {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: var(--space-2);
  }
  details {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    background: var(--surface-deep);
  }
  details[open] {
    background: var(--surface-sunken);
  }
  summary {
    cursor: pointer;
    padding: var(--space-2) var(--space-3);
    color: var(--text-primary);
    font-weight: 600;
    font-size: var(--font-size-sm);
    user-select: none;
    border-radius: var(--radius-md);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  summary:hover {
    background: var(--surface-1);
  }
  details[open] > summary {
    border-bottom: 1px solid var(--border-subtle);
    border-radius: var(--radius-md) var(--radius-md) 0 0;
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
    background: var(--surface-info);
    border: 1px solid var(--border-info);
    color: var(--text-info);
  }
  .kind-badge.shell {
    background: var(--surface-success);
    border: 1px solid var(--text-success-bright);
    color: var(--text-success);
  }
  .kind-badge {
    border-radius: var(--radius-pill);
  }
  .builtin-tag {
    display: inline-block;
    font-size: 9px;
    font-weight: var(--font-weight-medium);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-tertiary);
    border: 1px solid var(--border-default);
    padding: 1px 6px;
    border-radius: var(--radius-pill);
    margin-left: 6px;
    vertical-align: middle;
  }
  .shell-edit {
    padding: var(--space-3) 14px;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .shell-edit label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--font-size-sm);
    color: var(--text-quiet);
  }
  .shell-edit input[type="text"] {
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-md);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .shell-edit input[type="text"]:focus {
    outline: none;
    border-color: var(--accent);
  }
  .shell-edit input[disabled] {
    color: var(--text-tertiary);
    background: var(--surface-deep);
  }
  .shell-edit code {
    background: var(--surface-1);
    padding: 1px var(--space-1);
    border-radius: var(--radius-sm);
    font-size: var(--font-size-xs);
  }
</style>

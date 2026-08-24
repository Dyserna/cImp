<script lang="ts">
  /// Settings → Avatar (#129 (c)) — the on-screen mascot and the waveform drawn
  /// under it. Two `<section>`s under one nav entry, so one component: the
  /// waveform is the avatar's own overlay and every field it edits lives under
  /// `avatar.waveform`.
  ///
  /// Everything here is `snapshot` / `patch`. The three helpers that came with
  /// it are pure functions of the picker result — `imagePicker`, `pickTransition`
  /// and `basename` — and `pickFile` is imported directly, as `ToolPluginsSection`
  /// already does: it is a stateless one-shot dialog, not state this window owns.
  ///
  /// `themeWaveformColor` moved too. It is a DISPLAY default, not a setting:
  /// `avatar.waveform.color` stores `''` for "follow the theme", and an
  /// `<input type="color">` has no empty state, so the picker shows the theme's
  /// resolved `--waveform-color` instead. It reads `snapshot.ui.theme` only to
  /// re-run when the theme changes — `settings_main.ts` has already flipped
  /// `<html data-theme>` by then, so `getComputedStyle` answers for the new
  /// theme.
  import { SPRITE_SETS } from '../../avatarConfig';
  import { pickFile } from '../pickFile';
  import type { Settings } from '../types';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
  } = $props();

  /// Resolved theme default for the waveform color, used as the picker's
  /// displayed value when `avatar.waveform.color` is empty (the "follow
  /// theme" sentinel). Re-evaluates whenever `ui.theme` changes — the
  /// `<html data-theme>` attribute has already been updated by
  /// `settings_main.ts` so `getComputedStyle` reflects the new theme.
  const themeWaveformColor = $derived.by(() => {
    void snapshot.ui.theme;
    if (typeof window === 'undefined') return '#bb55ff';
    const v = getComputedStyle(document.documentElement)
      .getPropertyValue('--waveform-color')
      .trim();
    return v || '#bb55ff';
  });

  /// The per-state image/video picker. Curried so each row can hand its own
  /// state key in from the `{#each}`.
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

<section>
  <h2>Avatar</h2>
  <Toggle
    label="Visible"
    checked={snapshot.avatar.visible}
    onchange={(next) => patch((s) => (s.avatar.visible = next))}
  />
  <SelectField
    label="Type"
    value={snapshot.avatar.kind}
    onchange={(next) =>
      patch((s) => (s.avatar.kind = next as Settings['avatar']['kind']))}
  >
    <option value="media">Picture / Video</option>
    <option value="sprite">Animated sprites</option>
  </SelectField>
  {#if snapshot.avatar.kind === 'sprite'}
    <SelectField
      label="Sprite set"
      value={snapshot.avatar.sprite.set}
      onchange={(next) => patch((s) => (s.avatar.sprite.set = next))}
    >
      <!-- V40 Phase F: the bundled sets are named once, in
           `avatarConfig.ts` (locked decision 29 rules them brand
           assets, not harness identity). -->
      {#each SPRITE_SETS as set (set.id)}
        <option value={set.id}>{set.label}</option>
      {/each}
    </SelectField>
    <small class="hint">
      Frame-animated pixel-art mascot. Each state (Idle, Listening,
      Thinking, Speaking, Error) maps to a set of animations from the
      set's <code>manifest.json</code>; the per-state image/video and
      transition options below are ignored in this mode.
    </small>
  {/if}
  <SelectField
    label="Position"
    value={snapshot.avatar.position}
    onchange={(next) =>
      patch((s) => (s.avatar.position = next as Settings['avatar']['position']))}
  >
    <option value="top-right">Top Right</option>
    <option value="top-left">Top Left</option>
    <option value="bottom-right">Bottom Right</option>
    <option value="bottom-left">Bottom Left</option>
  </SelectField>
  <div class="row">
    <NumberField
      label="Width (px)"
      min="50"
      max="1200"
      value={snapshot.avatar.size.width_px}
      onchange={(next) =>
        patch((s) => (s.avatar.size.width_px = Math.max(50, +next)))}
    />
    <NumberField
      label="Height (px)"
      min="50"
      max="1200"
      value={snapshot.avatar.size.height_px}
      onchange={(next) =>
        patch((s) => (s.avatar.size.height_px = Math.max(50, +next)))}
    />
  </div>
  <div class="row">
    <NumberField
      label="Margin X (px)"
      min="0"
      max="200"
      value={snapshot.avatar.margin.x_px}
      onchange={(next) =>
        patch((s) => (s.avatar.margin.x_px = Math.max(0, +next)))}
    />
    <NumberField
      label="Margin Y (px)"
      min="0"
      max="200"
      value={snapshot.avatar.margin.y_px}
      onchange={(next) =>
        patch((s) => (s.avatar.margin.y_px = Math.max(0, +next)))}
    />
  </div>
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
  <Toggle
    label="Show border"
    checked={snapshot.avatar.show_border}
    onchange={(next) => patch((s) => (s.avatar.show_border = next))}
  />

  {#if snapshot.avatar.kind !== 'sprite'}
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
  <NumberField
    label="Duration (ms)"
    min="0"
    max="5000"
    step="50"
    value={snapshot.avatar.transition.duration_ms}
    onchange={(next) =>
      patch((s) => (s.avatar.transition.duration_ms = Math.max(0, +next)))}
  />
  {/if}
</section>

<section>
  <h2>Waveform</h2>
  <Toggle
    label="Show waveform"
    checked={snapshot.avatar.waveform.visible}
    onchange={(next) => patch((s) => (s.avatar.waveform.visible = next))}
  />
  <div class="file-row">
    <span class="state-label">Color</span>
    <input
      type="color"
      value={snapshot.avatar.waveform.color || themeWaveformColor}
      oninput={(e) =>
        patch((s) => (s.avatar.waveform.color = (e.currentTarget as HTMLInputElement).value))}
    />
    <button
      class="ghost"
      onclick={() => patch((s) => (s.avatar.waveform.color = ''))}
      disabled={snapshot.avatar.waveform.color === ''}
      title="Follow active UI theme"
    >
      Reset
    </button>
  </div>
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

<style>
  /* One file-picker row: state label | resolved filename | Pick… | Reset.
     Used by the per-state images, the transition path and the waveform colour
     row — all three of which live in this component now, so the rules travel
     with them (a Svelte class rule is scoped to whichever component holds the
     markup). */
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
</style>

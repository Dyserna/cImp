<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
  import { settings as settingsStore } from './store';
  import type { TerminalBackgroundSettings } from './types';

  // V1.4-03: extracted from SettingsApp.svelte's terminal-background
  // subsection so the per-tab Background row in ConfigureTabDialog and
  // TabSettingsSection can reuse the same controls. Two-way binds the
  // background config; persistence is the parent's responsibility (use
  // a getter/setter `bind:config` to thread mutations into applySettings,
  // reconfigure_shell_tab, or whichever path applies).
  let {
    config = $bindable<TerminalBackgroundSettings>(),
  }: {
    config: TerminalBackgroundSettings;
  } = $props();

  // V1.4-04 B.4: presets are read from the global settings store, not
  // from the bound `config`. The bound config can itself be a per-tab
  // override (which carries an empty `presets: []` for wire-format
  // reasons — see `BackgroundOverride` doc note in schema/ui.rs). The
  // user-facing preset library is global only.
  let presets = $derived($settingsStore.terminal.background.presets);

  // bgMode is *intent*; (config.image, config.color) are state. The
  // c6e3e8a pattern: derive bgMode once from the initial snapshot,
  // then let user clicks drive it. Without the `bgModeInitialised`
  // gate, an $effect re-derives the mode on every snapshot flush and
  // the UI snaps "Image" back to "Theme default" the moment the user
  // picks Image but hasn't chosen a file yet.
  let bgMode = $state<'theme' | 'color' | 'image'>('theme');
  let bgModeInitialised = false;
  $effect(() => {
    if (bgModeInitialised) return;
    bgMode = config.image ? 'image' : config.color ? 'color' : 'theme';
    bgModeInitialised = true;
  });

  function setBgMode(next: 'theme' | 'color' | 'image') {
    bgMode = next;
    if (next === 'theme') {
      config = { ...config, image: null, color: null };
    } else if (next === 'color') {
      config = {
        ...config,
        image: null,
        color: config.color ?? '#1a1a1a',
      };
    }
    // 'image': don't write yet — user picks file next via pickBgImage.
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
      config = { ...config, image: selected };
    }
  }

  function clearBgImage() {
    config = { ...config, image: null };
  }

  function clearBgTint() {
    config = { ...config, color: null };
  }

  function setOpacity(next: number) {
    config = { ...config, opacity: next };
  }

  function setBlur(next: number) {
    config = { ...config, blur: next };
  }

  function setSize(next: 'cover' | 'contain' | 'tile') {
    config = { ...config, size: next };
  }

  function setPosition(next: string) {
    config = { ...config, position: next };
  }

  function setColor(next: string) {
    config = { ...config, color: next };
  }

  // V1.4-04 B.4: one-shot preset picker. Selecting a preset overwrites
  // the shared subset of `config` with the preset's contents; the
  // bound config's existing `presets` array is preserved (presets only
  // live on the global config in practice, but the type carries the
  // field so we keep it intact). The select is reset to `''` after
  // each pick so the dropdown stays a verb ("Load preset…"), not a
  // state indicator — the mode select below is the source of truth
  // for what's currently configured.
  function loadPreset(name: string) {
    if (!name) return;
    const preset = presets.find((p) => p.name === name);
    if (!preset) return;
    // Preserve fields that aren't part of a preset payload:
    //   - `presets`: the global library lives on the global config; an
    //     override carries `[]`. Either way, loading a preset shouldn't
    //     change it.
    //   - `preview_category_flips` (V1.4-04 C.4): global UI behavior,
    //     not a preset property.
    config = {
      ...preset.config,
      presets: config.presets,
      preview_category_flips: config.preview_category_flips,
    };
    // Snap bgMode back into agreement with the loaded config. This is
    // an explicit user action — not a settings-store snapshot — so the
    // c6e3e8a guard against snapshot-driven re-derivation doesn't
    // apply. Treat it like a fresh pickBgMode click.
    bgMode = config.image ? 'image' : config.color ? 'color' : 'theme';
  }
</script>

<div class="bg-editor">
  {#if presets.length > 0}
    <label class="field">
      <span>Load preset</span>
      <select
        value=""
        onchange={(e) => {
          const target = e.currentTarget as HTMLSelectElement;
          loadPreset(target.value);
          target.value = '';
        }}
      >
        <option value="">Load preset…</option>
        {#each presets as p (p.name)}
          <option value={p.name}>{p.name}</option>
        {/each}
      </select>
    </label>
  {/if}

  <label class="field">
    <span>Background</span>
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
    <small class="hint">
      Solid color is rendered with no performance cost. Image switches to a
      slower DOM renderer (2-5× slower for high-throughput output). Toggling
      the image triggers a renderer flip, but your shell session and
      scrollback survive.
    </small>
  </label>

  {#if bgMode === 'color'}
    <div class="group">
      <h4>Color</h4>
      <label class="field">
        <span>Background color</span>
        <input
          type="color"
          value={config.color ?? '#1a1a1a'}
          onchange={(e) => setColor((e.currentTarget as HTMLInputElement).value)}
        />
        <small class="hint">Overrides the theme's background color.</small>
      </label>
    </div>
  {:else if bgMode === 'image'}
    <div class="group">
      <h4>Image</h4>
      <label class="field">
        <span>File</span>
        <span class="bg-image-row">
          <button type="button" onclick={pickBgImage}>Choose…</button>
          {#if config.image}
            <code class="bg-image-path" title={config.image}>{config.image}</code>
            <button type="button" onclick={clearBgImage}>Clear</button>
          {:else}
            <span class="bg-image-empty">No image selected.</span>
          {/if}
        </span>
      </label>
    </div>

    <div class="group">
      <h4>Appearance</h4>
      <label class="field">
        <span>Opacity: {config.opacity.toFixed(2)}</span>
        <input
          type="range"
          min="0"
          max="1"
          step="0.05"
          value={config.opacity}
          oninput={(e) =>
            setOpacity(+(e.currentTarget as HTMLInputElement).value)}
        />
      </label>
      <label class="field">
        <span>Blur: {config.blur}px</span>
        <input
          type="range"
          min="0"
          max="40"
          step="1"
          value={config.blur}
          oninput={(e) =>
            setBlur(+(e.currentTarget as HTMLInputElement).value)}
        />
      </label>
      <label class="field">
        <span>Tint color</span>
        <span class="bg-image-row">
          <input
            type="color"
            value={config.color ?? '#000000'}
            onchange={(e) => setColor((e.currentTarget as HTMLInputElement).value)}
          />
          {#if config.color}
            <button type="button" onclick={clearBgTint}>Reset</button>
          {/if}
        </span>
        <small class="hint">
          Tints the dimming overlay drawn beneath the cells. Defaults to
          black when unset.
        </small>
      </label>
    </div>

    <div class="group">
      <h4>Layout</h4>
      <label class="field">
        <span>Size</span>
        <select
          value={config.size}
          onchange={(e) =>
            setSize(
              (e.currentTarget as HTMLSelectElement).value as
                | 'cover'
                | 'contain'
                | 'tile',
            )}
        >
          <option value="cover">cover</option>
          <option value="contain">contain</option>
          <option value="tile">tile</option>
        </select>
      </label>
      <label class="field">
        <span>Position</span>
        <input
          type="text"
          placeholder="center"
          value={config.position}
          onchange={(e) =>
            setPosition((e.currentTarget as HTMLInputElement).value)}
        />
      </label>
    </div>
  {/if}
</div>

<style>
  .bg-editor {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
  }
  .field > span:first-child {
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    font-variant-numeric: tabular-nums;
    font-feature-settings: 'tnum';
  }
  .field input[type='text'],
  .field select {
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
  .field input[type='text']:focus,
  .field select:focus {
    outline: none;
    border-color: var(--accent);
  }
  .field input[type='range'] {
    width: 100%;
    accent-color: var(--accent);
  }
  .field input[type='color'] {
    height: 32px;
    width: 64px;
    padding: 0;
    border: 1px solid var(--border-default);
    background: var(--surface-2);
    border-radius: var(--radius-md);
  }
  .hint {
    display: block;
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    margin-top: var(--space-1);
    line-height: 1.4;
  }

  /* Sub-section grouping inside the editor — separates Image / Appearance
     / Layout in image mode so the six controls don't visually pile up. */
  .group {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding-top: var(--space-3);
    border-top: 1px solid var(--border-faint);
  }
  .group h4 {
    margin: 0 0 var(--space-1) 0;
    font-size: var(--font-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-tertiary);
  }

  .bg-image-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .bg-image-path {
    flex: 1 1 200px;
    min-width: 0;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-sm);
    padding: 4px var(--space-2);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-xs);
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .bg-image-empty {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    font-style: italic;
  }
  .bg-image-row button {
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-3);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    flex-shrink: 0;
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .bg-image-row button:hover {
    background: var(--surface-input);
    border-color: var(--border-strong);
  }
</style>

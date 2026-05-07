<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
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
</script>

<label class="palette-row">
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
</label>
<small class="hint">
  Solid color is rendered with no performance cost. Image switches to a
  slower DOM renderer (2-5× slower for high-throughput output). Toggling
  the image triggers a renderer flip, but your shell session and
  scrollback survive.
</small>

{#if bgMode === 'color'}
  <label>
    <span>Background color</span>
    <input
      type="color"
      value={config.color ?? '#1a1a1a'}
      onchange={(e) => setColor((e.currentTarget as HTMLInputElement).value)}
    />
  </label>
  <small class="hint">Overrides the theme's background color.</small>
{:else if bgMode === 'image'}
  <label>
    <span>Image file</span>
    <span class="bg-image-row">
      <button type="button" onclick={pickBgImage}>Choose…</button>
      {#if config.image}
        <code class="bg-image-path">{config.image}</code>
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
      value={config.opacity}
      oninput={(e) =>
        setOpacity(+(e.currentTarget as HTMLInputElement).value)}
    />
    <small>{config.opacity.toFixed(2)}</small>
  </label>
  <label>
    <span>Blur (px)</span>
    <input
      type="range"
      min="0"
      max="40"
      step="1"
      value={config.blur}
      oninput={(e) =>
        setBlur(+(e.currentTarget as HTMLInputElement).value)}
    />
    <small>{config.blur}</small>
  </label>
  <label>
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
  <label>
    <span>Position</span>
    <input
      type="text"
      placeholder="center"
      value={config.position}
      onchange={(e) =>
        setPosition((e.currentTarget as HTMLInputElement).value)}
    />
  </label>
  <label>
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
  </label>
  <small class="hint">
    Tints the dimming overlay drawn beneath the cells. Defaults to black
    when unset.
  </small>
{/if}

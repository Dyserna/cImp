<script lang="ts">
  // Five-square color preview for a named (or custom) terminal palette.
  // Used inline next to the palette dropdown so users can scan available
  // themes without opening the full Custom editor. Shows background,
  // foreground, red, green, blue — enough variety to differentiate the
  // bundled themes at a glance.

  import { resolveBundledTheme, BUNDLED_THEMES } from '../themes';
  import type { ThemeColorsWire } from './types';

  interface Props {
    name: string;
    custom?: ThemeColorsWire | null;
  }

  let { name, custom = null }: Props = $props();

  const swatchKeys = ['background', 'foreground', 'red', 'green', 'blue'] as const;

  let colors = $derived.by(() => {
    if (name === 'Custom' && custom) {
      // Merge over Default so missing keys still preview something.
      return { ...BUNDLED_THEMES.Default, ...custom } as Record<string, string>;
    }
    return resolveBundledTheme(name) as unknown as Record<string, string>;
  });
</script>

<span class="swatch" aria-hidden="true">
  {#each swatchKeys as key}
    <span
      class="square"
      style="background: {colors[key] ?? '#000'}"
      title={key}
    ></span>
  {/each}
</span>

<style>
  .swatch {
    display: inline-flex;
    gap: 2px;
    align-items: center;
    margin-left: var(--space-2);
    vertical-align: middle;
  }
  .square {
    display: inline-block;
    width: 12px;
    height: 12px;
    border-radius: 2px;
    border: 1px solid var(--border-subtle);
  }
</style>

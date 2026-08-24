<script lang="ts">
  // 22-color editor for a custom xterm.js palette. Used both in the
  // global Settings → Appearance section (V1.4-01 Phase 7) and inside
  // the per-tab Configure Tab dialog (Phase 8). Displays grouped color
  // pickers — Base, ANSI 8, Bright 8 — and emits the full updated
  // ThemeColorsWire on each change.
  //
  // Seed-from-current is handled by the parent: when a user first
  // switches to "Custom", the parent populates `value` from whichever
  // palette was previously active so the editor opens with sensible
  // starting colors instead of 22 black squares.

  import { REQUIRED_THEME_KEYS } from '../themes';
  import type { ThemeColorsWire } from './types';

  interface Props {
    value: ThemeColorsWire;
    onchange: (next: ThemeColorsWire) => void;
  }

  let { value, onchange }: Props = $props();

  // Group definitions used to render the editor. Order matches xterm's
  // documented ITheme shape; each group has its own header.
  const groups: { title: string; keys: readonly string[] }[] = [
    {
      title: 'Base',
      keys: [
        'foreground',
        'background',
        'cursor',
        'cursorAccent',
        'selectionBackground',
        'selectionForeground',
      ],
    },
    {
      title: 'ANSI',
      keys: [
        'black',
        'red',
        'green',
        'yellow',
        'blue',
        'magenta',
        'cyan',
        'white',
      ],
    },
    {
      title: 'Bright',
      keys: [
        'brightBlack',
        'brightRed',
        'brightGreen',
        'brightYellow',
        'brightBlue',
        'brightMagenta',
        'brightCyan',
        'brightWhite',
      ],
    },
  ];

  // Format the color label for display: split camelCase, capitalize.
  function labelFor(key: string): string {
    return key
      .replace(/([A-Z])/g, ' $1')
      .replace(/^./, (c) => c.toUpperCase());
  }

  function setColor(key: string, hex: string) {
    onchange({ ...value, [key]: hex });
  }

  // Sanity check: warn at dev time if a key in `groups` doesn't appear
  // in REQUIRED_THEME_KEYS (catches typos when adding new keys later).
  $effect(() => {
    const expected = new Set<string>(REQUIRED_THEME_KEYS as readonly string[]);
    for (const g of groups) {
      for (const k of g.keys) {
        if (!expected.has(k)) {
          console.warn(`CustomThemeEditor: unrecognized key "${k}"`);
        }
      }
    }
  });
</script>

<div class="custom-theme-editor">
  {#each groups as group}
    <fieldset>
      <legend>{group.title}</legend>
      <div class="grid">
        {#each group.keys as key}
          <label>
            <span>{labelFor(key)}</span>
            <input
              type="color"
              value={value[key] ?? '#000000'}
              onchange={(e) =>
                setColor(key, (e.currentTarget as HTMLInputElement).value)}
            />
          </label>
        {/each}
      </div>
    </fieldset>
  {/each}
</div>

<style>
  .custom-theme-editor {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin-top: var(--space-2);
    padding: var(--space-3);
    background: var(--surface-2);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
  }

  fieldset {
    border: none;
    padding: 0;
    margin: 0;
  }

  legend {
    font-size: var(--font-size-sm);
    font-weight: var(--font-weight-medium);
    color: var(--text-secondary);
    margin-bottom: var(--space-2);
    padding: 0;
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: var(--space-2);
  }

  /* #129 (a): these three used to be bare-element rules. `settings-chrome.css`
     now carries `label`, `label > span:first-child` and `input[type='color']`
     at the same specificity, and this component's CSS lands in the SHARED chunk
     which the settings window loads BEFORE that sheet — so a tie would go to
     the chrome sheet and this editor would lose its flex row, its swatch size
     and its label metrics. Each selector is raised just past the chrome's. */
  .custom-theme-editor label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--font-size-xs);
    color: var(--text-secondary);
    margin-bottom: 0;
  }

  .custom-theme-editor .grid label span {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* beats the chrome's `label > span:first-child` (0,2,2) at 0,3,2. Its
       `display: block` is left alone deliberately — this span is a flex item,
       so block is what it computed to anyway. */
    margin-bottom: 0;
    color: inherit;
    font-size: inherit;
  }

  .custom-theme-editor input[type='color'] {
    width: 28px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    background: transparent;
    cursor: pointer;
  }
</style>

<script lang="ts">
  // Bottom-bar quick-launch button for a built-in situational tool
  // (rustnet, broot). Each click opens a fresh closable Shell tab running
  // the tool's fixed command (V16) — press it as many times as you like and
  // close the tabs individually when done. A missing tool still opens the
  // tab and shows the standard "command not found" overlay.
  import { openToolTab, type ToolKind } from '../ipc';

  interface Props {
    tool: ToolKind;
    glyph: string;
    label: string;
  }
  const { tool, glyph, label }: Props = $props();

  async function launch() {
    try {
      await openToolTab(tool);
    } catch (e) {
      console.error(`open_tool_tab(${tool}) failed:`, e);
    }
  }
</script>

<button
  type="button"
  class="status-button"
  onclick={() => void launch()}
  title={label}
  aria-label={label}
>
  <span class="glyph" aria-hidden="true">{glyph}</span>
</button>

<style>
  /* Shell + focus ring: `.status-button` in `src/app.css`. State only here. */
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .glyph {
    font-size: 14px;
  }
</style>

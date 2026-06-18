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
  .status-button {
    appearance: none;
    background: transparent;
    border: 1px solid transparent;
    color: var(--text-secondary);
    cursor: pointer;
    width: 26px;
    height: 22px;
    border-radius: var(--radius-pill);
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 14px;
  }
</style>

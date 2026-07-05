<script lang="ts">
  // Bottom-bar button that opens the Note scratchpad tab. Sits in the
  // broot/rustnet quick-launch group. The Note tab is a singleton: clicking
  // when it's already open just re-activates it (the backend handles both
  // create-and-open and re-activate). Its content lives in the project's
  // .cimp/cimp.note.txt file and autosaves.
  import { openNoteTab } from '../ipc';
  import { setFocusedPaneActiveTab } from '../layout/store';

  async function open() {
    try {
      const id = await openNoteTab();
      // Reveal + focus the note tab wherever it already lives. On the
      // create path this is a no-op (the tab isn't in the layout store yet —
      // the backend's TabAdded/TabActivated events place and focus it); on
      // the singleton re-activate path it brings an already-open note tab
      // forward even when it sits in a non-focused pane (the backend's
      // out-of-pane ActiveTabChanged broadcast is intentionally ignored by
      // the frontend, so we focus it explicitly here).
      setFocusedPaneActiveTab(id);
    } catch (e) {
      console.error('open_note_tab failed:', e);
    }
  }
</script>

<button
  type="button"
  class="status-button"
  onclick={() => void open()}
  title="Open note"
  aria-label="Open note scratchpad tab"
>
  <span class="glyph" aria-hidden="true">📝</span>
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

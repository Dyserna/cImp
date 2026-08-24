<script lang="ts">
  // Bottom-bar button that opens the Note scratchpad tab. Sits in the
  // broot/rustnet quick-launch group. The Note tab is a singleton: clicking
  // when it's already open just re-activates it (the backend handles both
  // create-and-open and re-activate). Its content lives in the project's
  // .cimp/cimp.note.txt file and autosaves.
  import { openNoteTab } from '../ipc';
  import { revealTab } from '../tabs/visibility';

  async function open() {
    try {
      const id = await openNoteTab();
      // Reveal + focus the note tab wherever it already lives. On the
      // create path this is a no-op (the tab isn't in the layout store yet —
      // the backend's TabAdded/TabActivated events place and focus it); on
      // the singleton re-activate path it brings an already-open note tab
      // forward even when it sits in a non-focused pane (the backend's
      // out-of-pane ActiveTabChanged broadcast is intentionally ignored by
      // the frontend, so we focus it explicitly here). revealTab also
      // un-hides a UI-hidden note tab, re-inserting it into the focused
      // pane — a plain activate would no-op since hidden tabs live outside
      // the layout tree.
      revealTab(id);
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
  /* Shell + focus ring: `.status-button` in `src/app.css`. State only here. */
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .glyph {
    font-size: 14px;
  }
</style>

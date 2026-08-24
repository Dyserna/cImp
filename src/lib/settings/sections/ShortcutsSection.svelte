<script lang="ts">
  /// Settings → Keyboard controls (#129 (c)).
  ///
  /// Pure form: every row reads `snapshot.shortcuts` and writes through the
  /// window's own `patch()`, so there is no state here beyond the two row
  /// tables the loops iterate. Nothing else in the window read those tables.
  ///
  /// `ShortcutCapture`'s `bind:value={getter, setter}` is a function PAIR, not
  /// an in-place bind: the setter calls `patch()`, which clones, mutates and
  /// pushes exactly as everywhere else. It is the shape this section already
  /// used and it is unchanged here — the binding `draftSync` forbids is one
  /// that mutates the live snapshot, which this never does.
  import type { Settings } from '../types';
  import ShortcutCapture from '../ShortcutCapture.svelte';

  let {
    snapshot,
    patch,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no in-place bind).
    patch: (updater: (s: Settings) => void) => void;
  } = $props();

  // Shortcut rows rendered as loops — the numbered tab slots and the pane
  // actions are 16 near-identical <label> rows otherwise. Every key is a
  // `string | null` field of the shortcuts slice.
  type ShortcutKey = keyof Settings['shortcuts'];
  const TAB_SHORTCUT_ROWS: readonly (readonly [ShortcutKey, string])[] = [
    ['switch_to_tab_3', 'Switch to tab 3'],
    ['switch_to_tab_4', 'Switch to tab 4'],
    ['switch_to_tab_5', 'Switch to tab 5'],
    ['switch_to_tab_6', 'Switch to tab 6'],
    ['switch_to_tab_7', 'Switch to tab 7'],
    ['switch_to_tab_8', 'Switch to tab 8'],
    ['switch_to_tab_9', 'Switch to tab 9'],
    ['new_shell_tab', 'New shell tab'],
    ['close_tab', 'Close current tab'],
  ];
  const PANE_SHORTCUT_ROWS: readonly (readonly [ShortcutKey, string])[] = [
    ['focus_pane_left', 'Focus pane left'],
    ['focus_pane_right', 'Focus pane right'],
    ['focus_pane_up', 'Focus pane up'],
    ['focus_pane_down', 'Focus pane down'],
    ['split_pane_horizontal', 'Split pane (side by side)'],
    ['split_pane_vertical', 'Split pane (stacked)'],
    ['close_pane', 'Close focused pane'],
  ];
</script>

<section>
  <h2>Keyboard controls</h2>
  <label>
    <span>Open compose</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.open_compose,
        (v) => patch((s) => (s.shortcuts.open_compose = v))
      }
    />
  </label>
  <label>
    <span>Open compose with template picker</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.open_compose_picker,
        (v) => patch((s) => (s.shortcuts.open_compose_picker = v))
      }
    />
  </label>
  <label>
    <span>Submit compose</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.submit_compose,
        (v) => patch((s) => (s.shortcuts.submit_compose = v))
      }
    />
  </label>
  <label>
    <span>Cancel compose</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.cancel_compose,
        (v) => patch((s) => (s.shortcuts.cancel_compose = v))
      }
    />
  </label>
  <label>
    <span>Open settings</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.open_settings,
        (v) => patch((s) => (s.shortcuts.open_settings = v))
      }
    />
  </label>
  <h3>Tabs</h3>
  <label>
    <span>Switch to tab 1</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.switch_to_tab_1,
        (v) => patch((s) => (s.shortcuts.switch_to_tab_1 = v))
      }
    />
  </label>
  <label>
    <span>Switch to tab 2</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.switch_to_tab_2,
        (v) => patch((s) => (s.shortcuts.switch_to_tab_2 = v))
      }
    />
  </label>
  {#each TAB_SHORTCUT_ROWS as [key, label] (key)}
    <label>
      <span>{label}</span>
      <ShortcutCapture
        bind:value={
          () => snapshot.shortcuts[key],
          (v) => patch((s) => (s.shortcuts[key] = v))
        }
      />
    </label>
  {/each}

  <h3>Panes</h3>
  {#each PANE_SHORTCUT_ROWS as [key, label] (key)}
    <label>
      <span>{label}</span>
      <ShortcutCapture
        bind:value={
          () => snapshot.shortcuts[key],
          (v) => patch((s) => (s.shortcuts[key] = v))
        }
      />
    </label>
  {/each}

  <h3>Voice</h3>
  <label>
    <span>Push-to-talk (speech-to-text)</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.push_to_talk,
        (v) => patch((s) => (s.shortcuts.push_to_talk = v))
      }
    />
  </label>
  <small class="hint">
    Hold the chord to record, release to transcribe. Works only when
    speech-to-text is enabled. The default is bare
    <code>Ctrl+Shift</code> — a quick tap or a
    <code>Ctrl+Shift+&lt;key&gt;</code> chord won't trigger a recording.
  </small>
  <label>
    <span>Speak selection (text-to-speech)</span>
    <ShortcutCapture
      bind:value={
        () => snapshot.shortcuts.speak_selection,
        (v) => patch((s) => (s.shortcuts.speak_selection = v))
      }
    />
  </label>
  <small class="hint">
    Reads the active terminal's current selection aloud — the keyboard
    equivalent of Ctrl+right-click. Shows a "No text selected" notice
    when nothing is selected.
  </small>
</section>

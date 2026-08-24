<script lang="ts">
  /// Settings → Text-to-speech (#129 (c)) — the TTS engine, the behaviour
  /// toggles that decide when it speaks, and the segmenter's stability knobs.
  ///
  /// `voices` is a PROP, not a fetch here: `SettingsApp`'s `onMount` calls
  /// `list_voices` up front along with every other section's data, and moving
  /// it in here would make it fire on first view instead — a behaviour change
  /// the issue does not sanction.
  import type { ProcessingDevice, Settings } from '../types';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    voices,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// Kokoro voice names, from the parent's eager `list_voices`.
    voices: string[];
  } = $props();
</script>

<section>
  <h2>TTS</h2>
  <Toggle
    label="Enable text-to-speech"
    checked={snapshot.tts.enabled}
    onchange={(next) => patch((s) => (s.tts.enabled = next))}
  />
  <small class="hint top">
    Loads the Kokoro voice model. Turn off to unload it and free
    memory — no AI output is spoken while disabled. (To keep the model
    loaded but silence playback, use <em>Mute</em> instead.)
  </small>
  <SelectField
    label="Process on"
    value={snapshot.tts.device}
    disabled={!snapshot.tts.enabled}
    onchange={(next) => patch((s) => (s.tts.device = next as ProcessingDevice))}
  >
    <option value="gpu">GPU (fall back to CPU)</option>
    <option value="cpu">CPU</option>
  </SelectField>
  <small class="hint">
    Where Kokoro runs. <strong>GPU</strong> uses the graphics card and
    automatically falls back to CPU if none is available;
    <strong>CPU</strong> forces CPU. Switching reloads the model on the
    new device — no restart needed.
  </small>
  <SelectField
    label="Voice"
    value={snapshot.tts.voice}
    disabled={!snapshot.tts.enabled}
    onchange={(next) => patch((s) => (s.tts.voice = next))}
  >
    {#each voices as v}
      <option value={v}>{v}</option>
    {/each}
  </SelectField>
  <label>
    <span>Speed: {snapshot.tts.speed.toFixed(2)}×</span>
    <input
      type="range"
      min="0.5"
      max="2"
      step="0.05"
      value={snapshot.tts.speed}
      disabled={!snapshot.tts.enabled}
      oninput={(e) =>
        patch((s) => (s.tts.speed = +(e.currentTarget as HTMLInputElement).value))}
    />
  </label>
  <label>
    <span>Volume: {Math.round(snapshot.tts.volume * 100)}%</span>
    <input
      type="range"
      min="0"
      max="1"
      step="0.01"
      value={snapshot.tts.volume}
      disabled={!snapshot.tts.enabled}
      oninput={(e) =>
        patch((s) => (s.tts.volume = +(e.currentTarget as HTMLInputElement).value))}
    />
  </label>
  <Toggle
    label="Mute"
    checked={snapshot.tts.mute}
    disabled={!snapshot.tts.enabled}
    onchange={(next) => patch((s) => (s.tts.mute = next))}
  />
</section>

<section>
  <h2>Behavior</h2>
  <small class="hint">
    TTS is only stopped by Esc (or by switching tabs) — typing never
    interrupts speech.
  </small>
  <Toggle
    label="Auto-speak detected segments"
    checked={snapshot.behavior.auto_speak}
    onchange={(next) => patch((s) => (s.behavior.auto_speak = next))}
  />
  <Toggle
    label="Follow avatar visibility"
    checked={snapshot.behavior.follow_avatar}
    onchange={(next) => patch((s) => (s.behavior.follow_avatar = next))}
  />
  <small class="hint">
    When on, hiding the avatar mutes TTS and showing it unmutes —
    the Mute toggle tracks the avatar. Turn this off to control
    mute independently.
  </small>
  <Toggle
    label="Announce focused tab"
    checked={snapshot.behavior.announce_focused_tab}
    onchange={(next) => patch((s) => (s.behavior.announce_focused_tab = next))}
  />
  <small class="hint">
    Off by default — announcements (idle, awaiting permission, error,
    exit) only fire for background tabs. Turn on to hear them for the
    tab you're currently looking at as well.
  </small>
  <NumberField
    label="Announce idle only after working for … seconds"
    min="0"
    max="3600"
    step="10"
    value={snapshot.behavior.idle_announce_min_working_secs}
    onchange={(next) =>
      patch((s) => (s.behavior.idle_announce_min_working_secs = Math.max(0, Math.round(+next || 0))))}
  />
  <small class="hint">
    An idle announcement is skipped when the tab worked for less than
    this. 0 announces every idle. Permission, question and error
    announcements are never gated.
  </small>
  <Toggle
    label="Speak tagged TTS from background tabs"
    checked={snapshot.behavior.speak_background_tabs}
    onchange={(next) => patch((s) => (s.behavior.speak_background_tabs = next))}
  />
  <small class="hint">
    Off by default — tagged TTS segments (the spoken bits inside
    AI-tab output) only play for the active tab. Turn on to hear
    them from background tabs too. Announcements are unaffected.
  </small>
  <Toggle
    label="Copy on select"
    checked={snapshot.behavior.copy_on_select}
    onchange={(next) => patch((s) => (s.behavior.copy_on_select = next))}
  />
  <small class="hint">
    When on, text selected in any terminal is copied to the system
    clipboard automatically.
  </small>
  <Toggle
    label="Paste on right-click"
    checked={snapshot.behavior.paste_on_right_click}
    onchange={(next) => patch((s) => (s.behavior.paste_on_right_click = next))}
  />
  <small class="hint">
    When on, right-clicking inside any terminal pastes the system
    clipboard into the focused shell and suppresses the browser's
    default context menu.
  </small>
  <Toggle
    label="Speak selection on Ctrl+right-click"
    checked={snapshot.behavior.speak_selection_on_right_click}
    onchange={(next) => patch((s) => (s.behavior.speak_selection_on_right_click = next))}
  />
  <small class="hint">
    When on, Ctrl+right-clicking inside any terminal reads the
    selected text aloud through TTS. Holding Ctrl always suppresses
    paste, so the gesture never pastes the clipboard.
  </small>
  <Toggle
    label="Highlight selection while reading"
    checked={snapshot.tts.selection_highlight.enabled}
    onchange={(next) => patch((s) => (s.tts.selection_highlight.enabled = next))}
  />
  <small class="hint">
    While the selection is read aloud, it is recolored and the
    highlight recedes sentence-by-sentence as each is spoken. The
    sentence being read uses a distinct accent color; finished text
    returns to its original colors. Press Esc to cancel and restore.
  </small>
  <small class="hint">
    Uncheck "Custom" on any channel to leave it as the terminal's own
    palette color (e.g. tint only the background, keeping the original
    text color).
  </small>
  <div class="color-grid" class:disabled={!snapshot.tts.selection_highlight.enabled}>
    {#each [
      { key: 'unread_fg', custom: 'unread_fg_custom', label: 'Unread text' },
      { key: 'unread_bg', custom: 'unread_bg_custom', label: 'Unread background' },
      { key: 'reading_fg', custom: 'reading_fg_custom', label: 'Reading text' },
      { key: 'reading_bg', custom: 'reading_bg_custom', label: 'Reading background' },
    ] as ch (ch.key)}
      <div class="color-cell">
        <span class="color-cell-label">{ch.label}</span>
        <!-- #129 (b) left this row inline because `.checkbox.compact` was still
             scoped to SettingsApp, and a scoped rule cannot reach the markup
             Toggle renders. (c) moved the rule to `settings-chrome.css` — it
             styles a Toggle, so a section-local sheet would not have reached it
             either — and the row converts. -->
        <Toggle
          class="compact"
          label="Custom"
          checked={(snapshot.tts.selection_highlight as unknown as Record<string, boolean>)[ch.custom]}
          disabled={!snapshot.tts.selection_highlight.enabled}
          onchange={(next) =>
            patch((s) => ((s.tts.selection_highlight as unknown as Record<string, boolean>)[ch.custom] = next))}
        />
        <input
          type="color"
          value={(snapshot.tts.selection_highlight as unknown as Record<string, string>)[ch.key]}
          disabled={!snapshot.tts.selection_highlight.enabled ||
            !(snapshot.tts.selection_highlight as unknown as Record<string, boolean>)[ch.custom]}
          onchange={(e) =>
            patch((s) => ((s.tts.selection_highlight as unknown as Record<string, string>)[ch.key] = (e.currentTarget as HTMLInputElement).value))}
        />
      </div>
    {/each}
  </div>
  <Toggle
    label="Show selection-TTS controls in the status bar"
    checked={snapshot.tts.show_selection_controls}
    onchange={(next) => patch((s) => (s.tts.show_selection_controls = next))}
  />
  <small class="hint">
    Adds play / pause / restart / stop buttons to the bottom bar for
    reading the current terminal selection aloud (play has the same
    effect as Ctrl+right-click).
  </small>
  <label class="checkbox disabled">
    <input type="checkbox" checked={snapshot.behavior.fallback_silent} disabled />
    <span>Fallback silent on TTS error (always on in v1)</span>
  </label>
</section>

<section>
  <h2>Processing</h2>
  <small class="hint top">
    Stream-stability tuning for the segmenter. Increase if speech
    chops mid-sentence; decrease if reactions feel sluggish.
  </small>
  <div class="row">
    <NumberField
      label="Stability timeout (ms)"
      min="0"
      max="2000"
      step="10"
      value={snapshot.processing.stability_timeout_ms}
      onchange={(next) =>
        patch((s) => (s.processing.stability_timeout_ms = Math.max(0, +next)))}
    />
    <NumberField
      label="Max hold (ms)"
      min="50"
      max="5000"
      step="50"
      value={snapshot.processing.max_hold_ms}
      onchange={(next) =>
        patch((s) => (s.processing.max_hold_ms = Math.max(50, +next)))}
    />
  </div>
</section>

<style>
  /* Selection-highlight color pickers: two columns, each a label + a
     "Custom" toggle + the swatch. */
  .color-grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: var(--space-2) var(--space-3);
    margin: var(--space-2) 0;
  }
  .color-cell {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: 0.85em;
  }
  .color-cell-label {
    font-weight: 500;
  }
  .color-grid.disabled {
    opacity: 0.5;
  }
</style>

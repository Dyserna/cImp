<script lang="ts">
  /// Settings → Speech-to-text (#129 (c)) — V6-01's offline Whisper dictation.
  ///
  /// `models` and `devices` are PROPS: `SettingsApp`'s `onMount` calls
  /// `stt_list_models` / `stt_list_input_devices` up front, and this section
  /// must not turn those into first-view fetches. `STT_LANGUAGES` is a static
  /// table nothing else read, so it moved.
  import type { ProcessingDevice, Settings } from '../types';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    models,
    devices,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// `ggml-*.bin` files found under `models/`.
    models: string[];
    /// cpal input-device names.
    devices: string[];
  } = $props();

  // Common Whisper language hints offered in the dropdown ("auto" detects).
  const STT_LANGUAGES: { code: string; label: string }[] = [
    { code: 'auto', label: 'Auto-detect' },
    { code: 'en', label: 'English' },
    { code: 'es', label: 'Spanish' },
    { code: 'fr', label: 'French' },
    { code: 'de', label: 'German' },
    { code: 'it', label: 'Italian' },
    { code: 'pt', label: 'Portuguese' },
    { code: 'nl', label: 'Dutch' },
    { code: 'ru', label: 'Russian' },
    { code: 'zh', label: 'Chinese' },
    { code: 'ja', label: 'Japanese' },
    { code: 'ko', label: 'Korean' },
    { code: 'ar', label: 'Arabic' },
    { code: 'he', label: 'Hebrew' },
    { code: 'hi', label: 'Hindi' },
  ];
</script>

<section>
  <h2>Speech-to-text</h2>
  <small class="hint">
    Dictate by voice instead of typing. A fully offline Whisper model
    transcribes your speech into the compose overlay for review before
    you send it. Nothing leaves your machine.
  </small>
  <Toggle
    label="Enable speech-to-text"
    checked={snapshot.stt.enabled}
    onchange={(next) => patch((s) => (s.stt.enabled = next))}
  />
  <small class="hint">
    Shows a microphone button in the bottom bar and enables the
    push-to-talk shortcut. Requires a model in the <code>models/</code> folder.
  </small>

  <SelectField
    label="Model"
    value={snapshot.stt.model_file}
    onchange={(next) => patch((s) => (s.stt.model_file = next))}
  >
    {#if !models.includes(snapshot.stt.model_file)}
      <option value={snapshot.stt.model_file}>{snapshot.stt.model_file} (missing)</option>
    {/if}
    {#each models as m}
      <option value={m}>{m}</option>
    {/each}
  </SelectField>
  {#if !models.includes(snapshot.stt.model_file)}
    <small class="hint warn">
      Model <code>{snapshot.stt.model_file}</code> isn't in the
      <code>models/</code> folder. Download a ggml Whisper model (e.g.
      <code>ggml-small.bin</code>) from
      huggingface.co/ggerganov/whisper.cpp and drop it there.
    </small>
  {:else}
    <small class="hint">
      Drop additional <code>ggml-*.bin</code> files into the
      <code>models/</code> folder to add models. Changing the model
      reloads the engine on your next recording.
    </small>
  {/if}

  <SelectField
    label="Process on"
    value={snapshot.stt.device}
    onchange={(next) => patch((s) => (s.stt.device = next as ProcessingDevice))}
  >
    <option value="gpu">GPU (fall back to CPU)</option>
    <option value="cpu">CPU</option>
  </SelectField>
  <small class="hint">
    Where Whisper runs. <strong>GPU</strong> uses the graphics card and
    automatically falls back to CPU if none is available;
    <strong>CPU</strong> forces CPU. Takes effect on your next recording.
  </small>

  <SelectField
    label="Input device"
    value={snapshot.stt.input_device}
    onchange={(next) => patch((s) => (s.stt.input_device = next))}
  >
    <option value="">System default</option>
    {#if snapshot.stt.input_device && !devices.includes(snapshot.stt.input_device)}
      <option value={snapshot.stt.input_device}>{snapshot.stt.input_device} (not found)</option>
    {/if}
    {#each devices as d}
      <option value={d}>{d}</option>
    {/each}
  </SelectField>

  <SelectField
    label="Language"
    value={snapshot.stt.language}
    onchange={(next) => patch((s) => (s.stt.language = next))}
  >
    {#if !STT_LANGUAGES.some((l) => l.code === snapshot.stt.language)}
      <option value={snapshot.stt.language}>{snapshot.stt.language}</option>
    {/if}
    {#each STT_LANGUAGES as l}
      <option value={l.code}>{l.label}</option>
    {/each}
  </SelectField>

  <Toggle
    label="Translate to English"
    checked={snapshot.stt.translate_to_english}
    onchange={(next) => patch((s) => (s.stt.translate_to_english = next))}
  />
  <small class="hint">
    Transcribe non-English speech as English instead of verbatim.
  </small>

  <SelectField
    label="Record button mode"
    value={snapshot.stt.button_mode}
    onchange={(next) =>
      patch((s) => (s.stt.button_mode = next as 'toggle' | 'hold'))}
  >
    <option value="toggle">Toggle (click to start / stop)</option>
    <option value="hold">Hold (press and hold to record)</option>
  </SelectField>

  <small class="hint">
    The push-to-talk shortcut (hold to record) lives in
    <strong>Keyboard controls</strong>.
  </small>
</section>

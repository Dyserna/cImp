<script lang="ts">
  import type { HarnessInfo, SettingFieldView } from '../harness';
  import type { Settings } from './types';
  import { harnessRow } from './types';

  /// V40 Phase B — **the generic per-harness settings form** (locked
  /// decision 6).
  ///
  /// One component for every harness, driven by what its backend plugin
  /// DECLARES (`harness_list`'s `fields`). It replaced the hand-written
  /// controls this window used to carry per harness — a status-line checkbox
  /// for one, three local-provider text inputs for the same one, a provider
  /// auto-sync box for another — each of which was a second declaration of a
  /// setting Rust already described, and each of which a third harness would
  /// have needed a copy of.
  ///
  /// The consequence worth stating: a harness that adds a setting adds a row to
  /// its `settings_schema()` and nothing else. No markup here, no field on
  /// `Settings`, no migration.
  let {
    harness,
    snapshot,
    patch,
  }: {
    /// The harness whose declared fields this renders.
    harness: HarnessInfo;
    /// The live settings snapshot the values are read from.
    snapshot: Settings;
    /// The window's own settings mutator. Given the key and the new value, so
    /// this component never touches the store directly.
    patch: (harnessId: string, key: string, value: unknown) => void;
  } = $props();

  /// Fields the form renders. `json` values are written by cImp, not typed —
  /// see `SettingKind` — so they are stored and round-tripped but have no
  /// control here.
  const rendered = $derived(harness.fields.filter((f) => f.kind !== 'json'));

  /// The stored value for a field, or its declared default. Never `undefined`:
  /// an absent key is the ordinary case (the backend resolves the same default
  /// for it), and feeding `undefined` to an input would make the control
  /// uncontrolled on the first paint.
  function valueOf(field: SettingFieldView): unknown {
    const stored = harnessRow(snapshot, harness.id).ext?.[field.key];
    return stored === undefined ? field.default : stored;
  }

  function asBool(field: SettingFieldView): boolean {
    return valueOf(field) === true;
  }

  function asText(field: SettingFieldView): string {
    const v = valueOf(field);
    return typeof v === 'string' ? v : String(v ?? '');
  }

  function asNumber(field: SettingFieldView): number | '' {
    const v = valueOf(field);
    return typeof v === 'number' ? v : '';
  }

  /// Which secret fields are currently revealed. Per key, so showing one token
  /// does not reveal another.
  let revealed = $state<Record<string, boolean>>({});
</script>

{#if rendered.length > 0}
  <section>
    <h3>{harness.label}</h3>
    {#each rendered as field (field.key)}
      {#if field.kind === 'bool'}
        <label class="checkbox">
          <input
            type="checkbox"
            checked={asBool(field)}
            onchange={(e) =>
              patch(harness.id, field.key, (e.currentTarget as HTMLInputElement).checked)}
          />
          <span>{field.label}</span>
        </label>
      {:else if field.kind === 'enum'}
        <label>
          <span>{field.label}</span>
          <select
            value={asText(field)}
            onchange={(e) => patch(harness.id, field.key, (e.currentTarget as HTMLSelectElement).value)}
          >
            {#each field.options as option (option)}
              <option value={option}>{option}</option>
            {/each}
          </select>
        </label>
      {:else if field.kind === 'int'}
        <label>
          <span>{field.label}</span>
          <input
            type="number"
            value={asNumber(field)}
            oninput={(e) => {
              const raw = (e.currentTarget as HTMLInputElement).value;
              const n = Number.parseInt(raw, 10);
              if (Number.isFinite(n)) patch(harness.id, field.key, n);
            }}
          />
        </label>
      {:else if field.secret}
        <label>
          <span>{field.label}</span>
          <div class="input-with-action">
            <input
              type={revealed[field.key] ? 'text' : 'password'}
              value={asText(field)}
              oninput={(e) => patch(harness.id, field.key, (e.currentTarget as HTMLInputElement).value)}
            />
            <button
              type="button"
              class="secondary"
              onclick={() => (revealed = { ...revealed, [field.key]: !revealed[field.key] })}
            >
              {revealed[field.key] ? 'Hide' : 'Show'}
            </button>
          </div>
        </label>
      {:else}
        <label>
          <span>{field.label}</span>
          <input
            type="text"
            value={asText(field)}
            oninput={(e) => patch(harness.id, field.key, (e.currentTarget as HTMLInputElement).value)}
          />
        </label>
      {/if}
      {#if field.hint}
        <small class="hint">
          {field.hint}
          {#if field.spawn_baked}
            Baked in at launch — restart the tab (Tabs → Restart) to apply.
          {/if}
        </small>
      {/if}
    {/each}
  </section>
{/if}

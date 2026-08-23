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
    filter = () => true,
  }: {
    /// The harness whose declared fields this renders.
    harness: HarnessInfo;
    /// The live settings snapshot the values are read from.
    snapshot: Settings;
    /// The window's own settings mutator. Given the key and the new value, so
    /// this component never touches the store directly.
    patch: (harnessId: string, key: string, value: unknown) => void;
    /// Which of the harness's declared fields this instance owns (issue #109).
    ///
    /// One harness's `ext` rows can render on more than one page — the
    /// custom-provider rows belong on the custom-provider tab's page — and the
    /// caller decides the split from the DECLARATION
    /// (`SettingFieldView.provider_tab`), never from the key. Default: all of
    /// them, which is the single-page case every other caller wants.
    filter?: (field: SettingFieldView) => boolean;
  } = $props();

  /// Fields the form renders. `json` values are written by cImp, not typed —
  /// see `SettingKind` — so they are stored and round-tripped but have no
  /// control here.
  const rendered = $derived(harness.fields.filter((f) => f.kind !== 'json' && filter(f)));

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

  /// The options an `enum` select offers: what the plugin declared, plus the
  /// STORED value when the file holds one the declaration no longer lists.
  ///
  /// V40 review finding M-7. A `<select>` whose `value` matches no `<option>`
  /// renders the FIRST one, so a settings file carrying a retired enum value —
  /// a hand edit, or a value a newer build wrote — showed the user a setting
  /// they do not have, and any other interaction with the form would have
  /// silently saved that misreading back. Shown as itself and marked instead.
  function optionsOf(field: SettingFieldView): { value: string; stale: boolean }[] {
    const out = field.options.map((value) => ({ value, stale: false }));
    const current = asText(field);
    if (current !== '' && !field.options.includes(current)) {
      out.unshift({ value: current, stale: true });
    }
    return out;
  }

  /// The kinds this build knows how to render. Anything else is a field a NEWER
  /// cImp declared: it gets a note, not a control (V40 review F-5). Rendering it
  /// as a text box — which is what the `{:else}` used to do — would have written
  /// a `String` into a key whose declared kind is something else, and the parse
  /// boundary would then reset it to the declared default at some later
  /// out-of-band read, with a `tracing::warn!` as the only trace.
  const RENDERABLE = ['bool', 'enum', 'int', 'text', 'path'] as const;
  function renderable(field: SettingFieldView): boolean {
    return (RENDERABLE as readonly string[]).includes(field.kind);
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
            {#each optionsOf(field) as option (option.value)}
              <option value={option.value}
                >{option.value}{option.stale ? ' (stored, no longer offered)' : ''}</option
              >
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
              // V40 review M-7: an EMPTIED box is an edit, not a non-event. It
              // used to be dropped on the floor — `parseInt('')` is `NaN` — so
              // the control showed blank while the stored value stood, and the
              // user's clear was silently discarded. Clearing resets to the
              // declared default, which is the one value the backend also
              // resolves for an absent key.
              if (raw.trim() === '') {
                patch(harness.id, field.key, field.default);
                return;
              }
              const n = Number.parseInt(raw, 10);
              if (Number.isFinite(n)) patch(harness.id, field.key, n);
            }}
          />
        </label>
      {:else if !renderable(field)}
        <div class="unrenderable">
          <span>{field.label}</span>
          <small class="hint">
            This build cannot show this setting (declared as <code>{field.kind}</code>). Its
            stored value is kept exactly as it is — update cImp to edit it.
          </small>
        </div>
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
      <!-- V40 review M-7: the restart warning is NOT part of the hint. It used
           to be nested inside `{#if field.hint}`, so a declared field with no
           hint — an ordinary thing to declare — got no warning at all and its
           flip looked like it had taken effect. -->
      {#if field.hint || field.spawn_baked}
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

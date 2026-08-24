<script lang="ts">
  /// A settings select row: `<label><span>…</span><select>…</select></label>`
  /// (#129 (b)). Companion to `Toggle.svelte`; the same no-`bind:` contract
  /// applies — see that file for why.
  ///
  ///   <SelectField
  ///     label="Process on"
  ///     value={snapshot.tts.device}
  ///     disabled={!snapshot.tts.enabled}
  ///     onchange={(next) => patch((s) => (s.tts.device = next as TtsDevice))}
  ///   >
  ///     <option value="cpu">CPU</option>
  ///     <option value="gpu">GPU</option>
  ///   </SelectField>
  ///
  /// The options are the default children rather than a data prop: the call
  /// sites build them from `{#each}`, mark some `disabled`, and group a few, and
  /// an options array would have to grow a field for each of those.
  ///
  /// `onchange` gets the raw string; a site that stores a union type casts it,
  /// exactly as it did when the coercion was written out inline.
  import type { Snippet } from 'svelte';

  interface Props {
    /// Text for the label's `<span>`. An expression is fine — it is a string.
    label: string;
    value: string | number;
    /// Receives the selected option's value.
    onchange: (next: string) => void;
    disabled?: boolean;
    /// The `<option>` list.
    children: Snippet;
    /// Extra content after the select, inside the label (a trailing hint).
    after?: Snippet;
  }

  let { label, value, onchange, disabled = false, children, after }: Props = $props();
</script>

<label>
  <span>{label}</span>
  <select
    {value}
    {disabled}
    onchange={(e) => onchange((e.currentTarget as HTMLSelectElement).value)}
  >
    {@render children()}
  </select>
  {@render after?.()}
</label>

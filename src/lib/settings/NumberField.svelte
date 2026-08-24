<script lang="ts">
  /// A settings number row: `<label><span>…</span><input type="number"></label>`
  /// (#129 (b)). Companion to `Toggle.svelte`; the same no-`bind:` contract
  /// applies — see that file for why.
  ///
  ///   <NumberField
  ///     label="Max hold (ms)"
  ///     min="50"
  ///     max="5000"
  ///     step="50"
  ///     value={snapshot.processing.max_hold_ms}
  ///     onchange={(next) => patch((s) => (s.processing.max_hold_ms = Math.max(50, +next)))}
  ///   />
  ///
  /// `onchange` gets the input's RAW string, not a number. That is deliberate:
  /// the ~50 call sites parse and clamp differently (`+v`, `parseInt`,
  /// `Math.max(min, …)`, `|| 0`, empty string meaning "unset") and each of those
  /// is a decision about that setting, not a detail this component should own.
  /// Every site keeps the expression it already had.
  import type { Snippet } from 'svelte';

  interface Props {
    /// Text for the label's `<span>`. An expression is fine — it is a string.
    label: string;
    value: number | string;
    /// Receives the input's raw string value; parse/clamp at the call site.
    onchange: (next: string) => void;
    min?: string | number;
    max?: string | number;
    step?: string | number;
    placeholder?: string;
    disabled?: boolean;
    /// Which DOM event commits. `change` (the default) fires on blur/Enter;
    /// `input` fires per keystroke. NOT interchangeable — a field that commits
    /// per keystroke rewrites the value while you are still typing it — so this
    /// is per call site, matching whatever that site used before.
    event?: 'change' | 'input';
    /// Extra content after the input, inside the label (a trailing hint).
    children?: Snippet;
  }

  let {
    label,
    value,
    onchange,
    min,
    max,
    step,
    placeholder,
    disabled = false,
    event = 'change',
    children,
  }: Props = $props();

  const commit = (e: Event & { currentTarget: HTMLInputElement }) =>
    onchange(e.currentTarget.value);
</script>

<label>
  <span>{label}</span>
  <input
    type="number"
    {min}
    {max}
    {step}
    {placeholder}
    {value}
    {disabled}
    onchange={event === 'change' ? commit : undefined}
    oninput={event === 'input' ? commit : undefined}
  />
  {@render children?.()}
</label>

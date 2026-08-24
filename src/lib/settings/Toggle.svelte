<script lang="ts">
  /// A settings checkbox row: `<label class="checkbox"><input type="checkbox">
  /// <span>…</span></label>`, the shape the Settings window has used since the
  /// first version, said once instead of ~85 times (#129 (b)).
  ///
  /// It does NOT bind. `draftSync.ts` forbids in-place binding of settings
  /// values — mutating the live snapshot in place closed a reproduced
  /// lost-update race — so this component only reports the new value and the
  /// call site feeds it into its own `patch()` exactly as before:
  ///
  ///   <Toggle
  ///     label="Enable text-to-speech"
  ///     checked={snapshot.tts.enabled}
  ///     onchange={(next) => patch((s) => (s.tts.enabled = next))}
  ///   />
  ///
  /// The styling comes from `settings-chrome.css` (`label.checkbox`,
  /// `label.checkbox > span`, and the `label.checkbox + small.hint` sibling
  /// rule, which still applies because the rendered DOM is unchanged).
  import type { Snippet } from 'svelte';

  interface Props {
    checked: boolean;
    /// Receives the new checked state. Feed it into the call site's `patch()`.
    onchange: (next: boolean) => void;
    /// Plain-text label. Use `children` instead when the label carries markup.
    label?: string;
    /// Label content, for the rows whose text embeds `<strong>`/`<code>`.
    children?: Snippet;
    disabled?: boolean;
    /// Extra classes for the `<label>` (e.g. `disabled`). The `checkbox` class
    /// is always present.
    class?: string;
  }

  let {
    checked,
    onchange,
    label = '',
    children,
    disabled = false,
    class: extraClass = '',
  }: Props = $props();
</script>

<label class={extraClass ? `checkbox ${extraClass}` : 'checkbox'}>
  <input
    type="checkbox"
    {checked}
    {disabled}
    onchange={(e) => onchange((e.currentTarget as HTMLInputElement).checked)}
  />
  <!-- On one line on purpose: the span is styled `margin: 0` and nothing else,
       so any indentation here would show up as leading text inside it. -->
  <span>{#if children}{@render children()}{:else}{label}{/if}</span>
</label>

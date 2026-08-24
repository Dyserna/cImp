<script lang="ts" generics="T extends string">
  // The segmented sub-tab strip under an app-view's header, plus the
  // save half of its persistence.
  //
  // Extracted in #128 from `ToolActivityView`, `WorkbenchView`,
  // `CodeIntelligenceView` and `CodeAuditView`, which carried the same ten
  // markup lines and the same ~30 CSS lines four times over.
  //
  // The LOAD half deliberately stays at each call site:
  //
  //     let section = $state<Section>(loadViewSection(view, ids, fallback));
  //
  // The views read `section` in `$derived`s and section-gated fetches that run
  // before this component mounts, so the initial value has to be theirs — and
  // `loadViewSection` is synchronous precisely so it can sit in a `$state`
  // initialiser without a first-paint flash (see `viewSection.ts`, V42 Phase
  // C). What moves here is the save `$effect`, which every caller wrote
  // identically.
  //
  // Scoped-CSS note: a snippet is compiled in the component that DECLARES it,
  // so `trailing`'s markup carries the caller's style scope, not this one's.
  // That is what lets `CodeIntelligenceView` keep its `.badge` rule at home
  // while rendering the badge inside a button this component owns.
  import type { Snippet } from 'svelte';
  import { saveViewSection } from './viewSection';

  let {
    view,
    sections,
    section = $bindable(),
    onselect,
    trailing,
    layout = 'flow',
  }: {
    /// Persistence key — the same `view` string passed to `loadViewSection`.
    view: string;
    /// The strip, in display order.
    sections: readonly { id: T; label: string }[];
    /// The selected section. Two-way: the caller owns the initial value.
    section: T;
    /// Extra work on a click, run after the selection is written and on every
    /// click including a re-click of the active section (Code Intelligence
    /// refetches memory / usage this way).
    onselect?: (id: T) => void;
    /// Rendered immediately after a section's label, inside its button, with
    /// no whitespace between the two. Used for count badges.
    trailing?: Snippet<[T]>;
    /// Where the strip sits in its parent:
    ///   'flow'  — 14px of air before the content below (the default; three
    ///             of the four views scroll their content under it).
    ///   'inset' — no bottom margin, inset from the tab's side padding, for a
    ///             parent whose content below is a flex-filled positioned host
    ///             (`CodeAuditView`).
    layout?: 'flow' | 'inset';
  } = $props();

  $effect(() => saveViewSection(view, section));
</script>

<nav class="sections" class:inset={layout === 'inset'}>
  {#each sections as s (s.id)}
    <button
      type="button"
      class="seg"
      class:active={section === s.id}
      onclick={() => {
        section = s.id;
        onselect?.(s.id);
      }}
    >{s.label}{@render trailing?.(s.id)}</button>
  {/each}
</nav>

<style>
  /* Three literals below have no token with their value, and #128 forbids a
     visual change, so they stay literal rather than being snapped to the
     nearest token: the 14px gap under the strip, the 6px segment radius (the
     radius scale tops out at 4px), and the translucent-white hover wash (no
     surface token is translucent). Everything else is tokenised. */
  nav.sections {
    display: flex;
    gap: var(--space-1);
    margin-bottom: 14px;
    border-bottom: 1px solid var(--border-subtle);
    padding-bottom: var(--space-2);
    flex-wrap: wrap;
  }
  /* The shorthand resets the base rule's `margin-bottom` to 0 as well as
     applying the side inset — both halves of CodeAuditView's delta. */
  nav.sections.inset {
    margin: 0 var(--space-4);
  }
  .seg {
    padding: var(--space-1) var(--space-3);
    border-radius: 6px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--text-primary);
    font-size: var(--font-size-sm);
    cursor: pointer;
    opacity: 0.7;
  }
  .seg:hover {
    background: rgba(255, 255, 255, 0.06);
    opacity: 1;
  }
  .seg.active {
    background: var(--accent);
    color: var(--accent-fg);
    opacity: 1;
    border-color: var(--accent);
  }
</style>

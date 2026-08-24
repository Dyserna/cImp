<script lang="ts">
  // The backdrop + centred card that every dialog in `src/lib/dialog/` wears,
  // plus the window keydown handler all but one of them re-declared.
  //
  // Extracted in #128. The seven dialogs carried a byte-identical `.backdrop`,
  // a `.card` that differed only in `width` (and two `max-height` variants),
  // an `h2` that differed only in its bottom margin, and the same
  // Escape/Enter listener six times over.
  //
  // ------------------------------------------------------------------
  // Scoped CSS: what moved here, what deliberately did NOT, and why
  // ------------------------------------------------------------------
  //
  // Svelte 5 scopes a rule by appending the component's hash class to the
  // FIRST selector and `:where(.hash)` — zero specificity — to the rest. So
  // `.actions button` in a dialog compiles to
  // `.actions.svelte-x button:where(.svelte-x)`, i.e. (0,2,1). That number
  // matters, because the themes reach into these buttons from outside every
  // component: `html[data-theme="tui"] .actions button` is (0,2,2) and carries
  // no `!important`, so the TUI/nippon bracket-button look wins over the
  // dialog's own padding and fill by exactly one element selector. Change a
  // dialog rule's specificity and you silently restyle three themes.
  //
  // Moved here, at IDENTICAL specificity:
  //   .backdrop, .card, h2, .actions  — this component's own elements.
  //   .actions button                 — `.actions :global(button)` compiles to
  //   .actions button:focus-visible     `.actions.svelte-shell button`, which
  //                                     is (0,2,1) / (0,3,1): the same numbers
  //                                     the dialogs had.
  //
  // Left in the callers ON PURPOSE:
  //   .cancel, .primary, their :hover rules, button[disabled]
  //     These are bare class/element selectors on caller-owned buttons, so
  //     they still compile to `.cancel.svelte-caller` (0,2,0) and keep working
  //     through the `actions` snippet — a snippet is compiled in the component
  //     that DECLARES it, so its elements carry the caller's hash. Pulling
  //     them in here would have to spell them `.actions :global(.cancel)`,
  //     which is (0,3,0) and would start beating the themes' (0,2,2) — a
  //     visible change in the default TUI theme. Nine duplicated lines per
  //     dialog is the price of not moving a theme boundary in a commit whose
  //     contract is "no visual change".
  //
  // Focus is likewise NOT owned here: each dialog focuses a different element
  // by a different mechanism (an `autofocus` attribute, a `use:` action, a
  // `queueMicrotask` against its own `bind:this`), so there is nothing shared
  // to lift — only four unrelated implementations that happen to run at open.
  import { onMount, type Snippet } from 'svelte';

  let {
    open,
    label,
    title,
    width,
    fit = 'auto',
    titleGap = 'lg',
    onCancel,
    onEscape,
    onEnter,
    children,
    actions,
  }: {
    /// Render the dialog. The listener below is registered for the component's
    /// whole life (dialogs are mounted permanently by `App.svelte`) and guards
    /// on this, exactly as the per-dialog handlers did.
    open: boolean;
    /// `aria-label` on the card.
    label: string;
    /// The `<h2>`.
    title: string;
    /// Card width in px. Applied through a custom property so the rule keeps
    /// the specificity it had as a plain `width:` declaration.
    width: number;
    /// 'scroll' — cap the height and scroll the card (RestoreCheckpoint).
    /// 'column' — cap the height and make the card a flex column so a list
    ///            inside it can take the slack (ManagePresets).
    fit?: 'auto' | 'scroll' | 'column';
    /// Gap under the title: 'lg' is `--space-4`, 'md' is `--space-3`. Four
    /// dialogs used one and three used the other; #128 forbids a visual
    /// change, so the four pixels are a prop rather than a decision.
    titleGap?: 'md' | 'lg';
    /// Backdrop click.
    onCancel: () => void;
    /// Escape. Omit for a dialog that does not answer Escape at all
    /// (RestoreCheckpoint never did) — `preventDefault` is then not called
    /// either. A caller that has to consume Escape for an inner state (an
    /// inline rename) branches INSIDE this callback, which is why there is no
    /// second Escape handler anywhere to order against.
    onEscape?: () => void;
    /// Enter, unless the event target is a BUTTON. Omit for a dialog with no
    /// single submit action.
    onEnter?: () => void;
    /// The card body, between the title and the actions row.
    children: Snippet;
    /// The action buttons. Rendered inside this component's `.actions` row but
    /// styled by the caller — see the scoped-CSS note above.
    actions: Snippet;
  } = $props();

  function onKeyDown(e: KeyboardEvent): void {
    if (!open) return;
    if (e.key === 'Escape') {
      if (!onEscape) return;
      e.preventDefault();
      onEscape();
    } else if (e.key === 'Enter' && (e.target as HTMLElement)?.tagName !== 'BUTTON') {
      // Enter on any input submits — convention for short modal forms.
      if (!onEnter) return;
      e.preventDefault();
      onEnter();
    }
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if open}
  <div class="backdrop" onclick={onCancel} role="presentation"></div>
  <div
    class="card"
    class:scroll={fit === 'scroll'}
    class:column={fit === 'column'}
    class:tight={titleGap === 'md'}
    role="dialog"
    aria-label={label}
    style="--modal-width: {width}px"
  >
    <h2>{title}</h2>
    {@render children()}
    <div class="actions">{@render actions()}</div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .card {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    padding: 20px var(--space-5);
    width: var(--modal-width);
    max-width: calc(100vw - 40px);
    color: var(--text-primary);
    z-index: 101;
    box-shadow: var(--shadow-lg);
  }
  .card.scroll {
    max-height: calc(100vh - 80px);
    overflow-y: auto;
  }
  .card.column {
    max-height: calc(100vh - 80px);
    display: flex;
    flex-direction: column;
  }
  h2 {
    margin: 0 0 var(--space-4);
    font-size: 16px;
    font-weight: 600;
  }
  .card.tight h2 {
    margin-bottom: var(--space-3);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  /* `:global` because these buttons come from the caller's `actions` snippet
     and therefore carry the CALLER's scope class, not this component's. The
     compiled selectors are `.actions.svelte-shell button` (0,2,1) and
     `… button:focus-visible` (0,3,1) — the exact numbers the per-dialog rules
     had, so the themes' (0,2,2) overrides still win where they did before. */
  .actions :global(button) {
    padding: 6px var(--space-4);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-md);
    border: 1px solid var(--border-default);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .actions :global(button):focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>

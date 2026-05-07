<script lang="ts">
  // Reusable tag/badge primitive (V5-02). Use for kind labels, severity
  // tags, restart-required indicators, and other small status pills.
  // The variant set covers the common cases: a neutral "default" pill,
  // accent/coral/orange semantic tags with matching outlines, and a solid
  // accent fill ("accent-fill") for the strong active-filter pattern.

  import type { Snippet } from 'svelte';

  type Variant = 'default' | 'accent' | 'coral' | 'orange' | 'accent-fill';
  type Size = 'xs' | 'sm' | 'md';

  let {
    variant = 'default',
    size = 'sm',
    children,
  }: {
    variant?: Variant;
    size?: Size;
    children: Snippet;
  } = $props();
</script>

<span class="pill pill-{variant} pill-{size}">{@render children()}</span>

<style>
  .pill {
    display: inline-flex;
    align-items: center;
    border-radius: var(--radius-pill);
    border: 1px solid transparent;
    font-weight: var(--font-weight-medium);
    line-height: 1;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    vertical-align: middle;
  }
  .pill-xs {
    font-size: 10px;
    padding: 2px var(--space-2);
  }
  .pill-sm {
    font-size: var(--font-size-xs);
    padding: 3px var(--space-2);
  }
  .pill-md {
    font-size: var(--font-size-sm);
    padding: var(--space-1) var(--space-3);
  }

  .pill-default {
    background: var(--surface-3);
    color: var(--text-secondary);
    border-color: var(--border-default);
  }
  .pill-accent {
    background: var(--accent-muted);
    color: var(--accent);
    border-color: var(--accent);
  }
  .pill-coral {
    background: var(--surface-danger-bg);
    color: var(--danger);
    border-color: var(--border-danger);
  }
  .pill-orange {
    background: var(--surface-warning-deep);
    color: var(--warning);
    border-color: var(--border-warning);
  }
  .pill-accent-fill {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
  }
</style>

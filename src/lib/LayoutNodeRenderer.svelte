<script lang="ts">
  // Recursive layout-tree renderer. Discriminates on `node.type` and
  // delegates to either Pane or Split. Pulled into its own module so
  // Split.svelte can reference it without forming a Svelte-internal
  // circular import (Pane → LayoutNodeRenderer → Split → renderer ...).

  import Pane from './Pane.svelte';
  import Split from './Split.svelte';
  import type { LayoutNode } from './layout/types';

  let { node }: { node: LayoutNode } = $props();
</script>

{#if node.type === 'pane'}
  <Pane pane={node} />
{:else}
  <Split split={node} />
{/if}

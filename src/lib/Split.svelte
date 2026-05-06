<script lang="ts">
  // Internal node of the layout tree: two children with a divider
  // between them. M1 renders the divider but it isn't draggable yet —
  // the resize handler lands in M3.

  import LayoutNodeRenderer from './LayoutNodeRenderer.svelte';
  import type { SplitNode } from './layout/types';

  let { split }: { split: SplitNode } = $props();
</script>

<div
  class="split"
  class:horizontal={split.direction === 'horizontal'}
  class:vertical={split.direction === 'vertical'}
>
  <div class="split-child" style:flex={`${split.ratio} 1 0%`}>
    <LayoutNodeRenderer node={split.first} />
  </div>
  <div class="splitter" aria-hidden="true"></div>
  <div class="split-child" style:flex={`${1 - split.ratio} 1 0%`}>
    <LayoutNodeRenderer node={split.second} />
  </div>
</div>

<style>
  .split {
    display: flex;
    flex: 1 1 auto;
    min-width: 0;
    min-height: 0;
    width: 100%;
    height: 100%;
  }
  .split.horizontal {
    flex-direction: row;
  }
  .split.vertical {
    flex-direction: column;
  }
  .split-child {
    display: flex;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
  }
  .splitter {
    background: #2a2a2a;
    flex-shrink: 0;
  }
  .split.horizontal > .splitter {
    width: 4px;
    cursor: col-resize;
  }
  .split.vertical > .splitter {
    height: 4px;
    cursor: row-resize;
  }
  .splitter:hover {
    background: #4a90e2;
  }
</style>

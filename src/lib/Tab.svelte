<script lang="ts">
  // Single tab button. v2-01 ships with active/inactive styling only;
  // status indicators (working dot, awaiting permission, error, done-while-
  // away) come in V2-03.
  let { label, active = false, onclick }: {
    label: string;
    active?: boolean;
    onclick: () => void;
  } = $props();
</script>

<button
  type="button"
  class="tab"
  class:active
  {onclick}
  aria-pressed={active}
>
  {label}
</button>

<style>
  .tab {
    appearance: none;
    border: none;
    background: transparent;
    color: #c0c0c0;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 13px;
    padding: 0 16px;
    height: 100%;
    cursor: pointer;
    border-right: 1px solid #2a2a2a;
    border-bottom: 2px solid transparent;
    line-height: 30px;
    user-select: none;
  }
  .tab:hover {
    background: #303030;
    color: #e0e0e0;
  }
  .tab.active {
    color: #ffffff;
    background: #1f1f1f;
    border-bottom-color: #4a90e2;
  }
</style>

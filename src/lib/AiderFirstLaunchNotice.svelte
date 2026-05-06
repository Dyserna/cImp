<script lang="ts">
  import { get } from 'svelte/store';
  import { activeTab } from './tabs/state';
  import { settings, applySettings } from './settings/store';
  import { AIDER_TAB_ID, findTab, findTabIndex } from './settings/types';

  // One-time notice shown the first time the user activates the aider tab,
  // explaining that TTS output is silent because aider has no system-prompt
  // injection mechanism yet (see docs/FUTURE-FEATURES.md). Dismissal sets
  // the aider tab entry's `first_launch_notice_dismissed` flag so the notice
  // never reappears.

  const visible = $derived.by(() => {
    if ($activeTab !== AIDER_TAB_ID) return false;
    const aider = findTab($settings, AIDER_TAB_ID);
    return !!aider && aider.kind === 'ai_tool' && !aider.first_launch_notice_dismissed;
  });

  function dismiss() {
    const current = get(settings);
    const next = structuredClone(current);
    const idx = findTabIndex(next, AIDER_TAB_ID);
    if (idx < 0) return;
    const entry = next.tabs[idx];
    if (entry.kind !== 'ai_tool') return;
    next.tabs[idx] = { ...entry, first_launch_notice_dismissed: true };
    void applySettings(next);
  }
</script>

{#if visible}
  <div class="overlay" role="dialog" aria-modal="true" aria-labelledby="aider-notice-title">
    <div class="card">
      <h2 id="aider-notice-title">About the Aider Tab</h2>
      <p>
        This tab runs aider, an alternative AI coding assistant. Tab status
        indicators, notifications, and visual feedback all work normally.
      </p>
      <p>
        Spoken TTS output is currently limited because aider does not yet
        support system-prompt injection via CLI. When that feature lands
        upstream, cctts will pick it up automatically. See
        <code>docs/FUTURE-FEATURES.md</code> for the action plan.
      </p>
      <div class="actions">
        <button class="primary" onclick={dismiss}>Got it</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(0, 0, 0, 0.6);
    z-index: 6;
    padding: 24px;
    box-sizing: border-box;
  }
  .card {
    max-width: 520px;
    width: 100%;
    background: #1f1a2a;
    border: 1px solid #6f42a8;
    color: #e0d8f0;
    border-radius: 6px;
    padding: 18px 20px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  }
  h2 {
    margin: 0 0 12px 0;
    font-size: 14px;
    font-weight: 600;
    color: #d8b8ff;
  }
  p {
    margin: 0 0 12px 0;
    font-size: 12px;
    line-height: 1.5;
  }
  code {
    background: #150f1f;
    padding: 1px 4px;
    border-radius: 3px;
    font-size: 11px;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    margin-top: 8px;
  }
  button.primary {
    background: #6f42a8;
    color: #fff;
    border: 1px solid #6f42a8;
    padding: 6px 18px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
  }
  button.primary:hover {
    background: #835ac5;
  }
</style>

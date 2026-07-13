import { invoke } from '@tauri-apps/api/core';
import type { AiToolTabConfig, HarnessVersions, LlmPricingModel, Settings } from './types';
import type { TabId } from '../tabs/types';

export async function settingsGet(): Promise<Settings> {
  return invoke('settings_get');
}

export async function settingsUpdate(settings: Settings): Promise<void> {
  await invoke('settings_update', { settings });
}

export async function listVoices(): Promise<string[]> {
  return invoke('list_voices');
}

export async function openSettingsWindow(): Promise<void> {
  await invoke('open_settings_window');
}

/// V1.4-07 A: open the Settings window scrolled to a specific tab's
/// section. Used by the right-click "Configure tab" entry on AI tabs
/// (shell tabs continue to use ConfigureTabDialog.svelte).
export async function openSettingsWindowToTab(tab: TabId): Promise<void> {
  await invoke('open_settings_window_to_tab', { tab });
}

/// V1.4-07 A: read+clear any pending deep-link target stored by
/// `open_settings_window_to_tab`. The Settings window calls this on
/// mount to handle the cold-open case (window not yet listening for
/// the `settings-deep-link` event when the IPC fires).
export async function consumeSettingsDeepLink(): Promise<string | null> {
  return invoke('consume_settings_deep_link');
}

export async function closeSettingsWindow(): Promise<void> {
  await invoke('close_settings_window');
}

/// V22 Phase E: open the Settings window scrolled to a top-level sidebar
/// section (not a tab). The Code Intelligence "suggested checks" nudge chip
/// uses this to jump straight to the `checks` editor.
export async function openSettingsWindowToSection(section: string): Promise<void> {
  await invoke('open_settings_window_to_section', { section });
}

export async function requestTabRestart(tab: TabId): Promise<void> {
  await invoke('request_tab_restart', { tab });
}

/// Per-AI-tab default config from the backend. Used by the Settings
/// window's "Reset to default" buttons to match the Rust-side defaults
/// exactly. Returns an error from the backend for non-AI ids.
export async function aiToolTabDefaults(tab: TabId): Promise<AiToolTabConfig> {
  return invoke('ai_tool_tab_defaults', { tab });
}

/// Activate a tab AND persist its id as `session.active_tab_id` so it's
/// restored on next launch. Backend debounces the save.
export async function setActiveTab(tab: TabId): Promise<void> {
  await invoke('set_active_tab', { tab });
}

/// The global LLM price table ($ per MTok per provider/model), read straight
/// from the physical global `settings.json` (missing file/key → the backend's
/// seeded Anthropic/Copilot defaults). Used by the Code Intelligence tab's
/// session-cost popup and the Settings → LLM pricing editor.
export async function llmPricingGet(): Promise<LlmPricingModel[]> {
  return invoke('llm_pricing_get');
}

/// Save the LLM price table straight to the physical global `settings.json`
/// — NOT through `settingsUpdate`'s per-project overlay diff (an array field
/// would land in the project overlay instead of global). Mirror of
/// `composeTemplatesGlobalSet`.
export async function llmPricingSet(pricing: LlmPricingModel[]): Promise<void> {
  await invoke('llm_pricing_set', { pricing });
}

/// V16 Feature 1: the harness version + contract state, read fresh from the
/// physical global `settings.json` — the settings snapshot only reflects app
/// startup, but `harness_versions` is written out-of-band (transcript tap,
/// hand edits per MAINTENANCE.md). Used by the Settings window so the E1
/// hard block reflects a just-recorded outcome without an app restart.
export async function harnessVersionsGet(): Promise<HarnessVersions> {
  return invoke('harness_versions_get');
}

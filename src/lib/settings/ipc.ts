import { invoke } from '@tauri-apps/api/core';
import type {
  AiToolTabConfig,
  AuditDetectResult,
  HarnessStatus,
  LlmPricingModel,
  Settings,
} from './types';
import type { TabId } from '../tabs/types';
import type { PluginSet } from './toolPlugins';

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
/// seeded vendor/Copilot defaults). Used by the Code Intelligence tab's
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
///
/// V35 Phase E: the payload also carries `capability_gates` — the gate verdicts
/// computed in Rust against those fresh versions, so the window renders an
/// answer instead of re-deriving one.
/// V35 Phase G: the payload also carries `harness_health` — the whole
/// *Harness health* read-model (every capability's tier, contract, degradation,
/// coverage, TCB marks, gate verdict and last check result) plus
/// `verify_in_flight`. Re-called on a short timer while a run is in flight.
export async function harnessVersionsGet(): Promise<HarnessStatus> {
  return invoke('harness_versions_get');
}

/// V35 Phase G: the *Harness health* panel's one action — run this harness's
/// L1 canaries and L2 probes now. `harness` is the token the panel renders
/// (`HarnessHealth.harness`).
///
/// Returns whether a run STARTED; `false` means one was already in flight and
/// the click was dropped (single-flight, shared with the automatic
/// version-change trigger). Fire-and-forget: the answers arrive through the
/// next `harnessVersionsGet`, not from this call.
export async function harnessRunChecks(harness: string): Promise<boolean> {
  return invoke('harness_run_checks', { harness });
}

/// Resolve one registered tool and probe `<tool> --version`.
///
/// `toolKey` is the registry key (`cimp-audit@1/gitleaks` for a built-in
/// scanner, `name@version/tool-id` for a user plugin's tool); `path` is the LIVE
/// value from the Settings input, passed explicitly so a just-typed path cannot
/// race the fire-and-forget settings push. An empty `path` means "resolve the
/// way this tool normally resolves", which for a built-in is its command name
/// through `ebin` then `PATH` and for a plugin is nowhere at all.
///
/// Display-only: the button renders the result inline and never writes the
/// resolved path back into the stored config.
export async function auditDetectTool(
  toolKey: string,
  path: string,
): Promise<AuditDetectResult> {
  return invoke('audit_detect_tool', { toolKey, path });
}

/// V38 Phase B: the current plugin set — what loaded from `<exe-dir>/plugins/`
/// and what did not. A READ of the state the startup scan (or the last Rescan)
/// produced, so opening Settings is never a disk walk.
export async function pluginsSnapshot(): Promise<PluginSet> {
  return invoke('plugins_snapshot');
}

/// Re-scan the plugins folder and return the new set. Mints the scan's `plugin`
/// Events rows backend-side, so a rejection shows up in the feed as well as in
/// the settings list.
export async function pluginsRescan(): Promise<PluginSet> {
  return invoke('plugins_rescan');
}

/// The key this project's per-tool binary path overrides are stored under.
/// Canonicalizing a path touches the disk, so the backend owns the rule and the
/// window asks for the answer — see `plugins::registry::project_key`.
export async function pluginsProjectKey(): Promise<string> {
  return invoke('plugins_project_key');
}

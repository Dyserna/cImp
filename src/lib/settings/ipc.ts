import { invoke } from '@tauri-apps/api/core';
import type { Settings } from './types';

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

export async function closeSettingsWindow(): Promise<void> {
  await invoke('close_settings_window');
}

export async function requestClaudeCodeRestart(): Promise<void> {
  await invoke('request_claude_code_restart');
}

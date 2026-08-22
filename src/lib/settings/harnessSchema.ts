/// V40 Phase B — **the registry's declared per-harness settings, over IPC**.
///
/// Locked decision 6. The Settings window used to carry a hand-written control
/// for every per-harness setting: a Claude status-line checkbox in one section,
/// three `claude_local` text inputs in another, an OpenCode provider block in a
/// third, and two "Expose to Claude Code / Expose to OpenCode" pairs. All of it
/// was a second declaration of the roster — so a harness's new setting cost
/// markup here as well as a field in Rust, and a third harness cost a new
/// checkbox everywhere a pair appeared.
///
/// This is the one place the window learns what a harness has. Fetched once
/// when the window opens; the payload is `'static` backend data that cannot go
/// stale between calls, which is why there is no refresh path.
///
/// Locked decision 7's fuller `harness_list` (tab ids, binaries, features,
/// consumer) is Phase F's, and will subsume this. What is here is what Phase B
/// needs and nothing more: the id, the display label, and the declared fields.
import { invoke } from '@tauri-apps/api/core';
import { writable } from 'svelte/store';

/// What a declared field HOLDS — mirrors Rust `harness::plugin::SettingKind`.
///
/// `json` is the escape hatch for a value cImp itself writes (OpenCode's
/// derived `local-llama` provider block): stored, round-tripped, and
/// deliberately NOT rendered, because its shape is the plugin's business.
export type SettingKind = 'bool' | 'int' | 'text' | 'path' | 'enum' | 'json';

/// One declared `ext` field. Mirror of Rust `ipc::commands::SettingFieldView`.
export interface SettingFieldView {
  /// The key inside `Settings.harness[<id>].ext`.
  key: string;
  kind: SettingKind;
  /// Allowed values for `kind === 'enum'`; empty otherwise.
  options: string[];
  label: string;
  /// One sentence under the control. May be empty.
  hint: string;
  /// The value an absent key reads as — what the form shows before the user has
  /// ever touched it.
  default: unknown;
  /// Flipping it needs a tab restart; the form says so.
  spawn_baked: boolean;
  /// A credential: the form masks it behind a Show/Hide button.
  secret: boolean;
}

/// One harness's declared settings. Mirror of Rust
/// `ipc::commands::HarnessSchemaView`.
export interface HarnessSchemaView {
  /// The registry id — the key into `Settings.harness` and into a server's
  /// `access` map.
  id: string;
  /// The display name for section headers and exposure checkboxes.
  label: string;
  /// Declared `ext` fields, in declaration order. Empty is an ordinary answer:
  /// such a harness gets an empty section and no UI work at all.
  fields: SettingFieldView[];
}

/// The registered harnesses, once the backend has answered.
///
/// Starts EMPTY rather than with a guessed roster: a synchronous fallback here
/// would be the frontend re-declaring the registry, which is the thing this
/// module removes. Every consumer renders nothing until it fills, which is one
/// paint at most — [`loadHarnessSchemas`] runs on window open.
export const harnessSchemas = writable<HarnessSchemaView[]>([]);

/// Fetch the declared schema. Best-effort: a failure leaves the store empty and
/// logs, because a Settings window that refuses to open over a harness list is
/// worse than one missing a section.
export async function loadHarnessSchemas(): Promise<void> {
  try {
    const list = await invoke<HarnessSchemaView[]>('harness_settings_schema');
    harnessSchemas.set(list ?? []);
  } catch (e) {
    console.error('harness_settings_schema failed:', e);
  }
}

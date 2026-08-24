import { open } from '@tauri-apps/plugin-dialog';

/// One native "choose a file" dialog, returning the chosen path or `null`.
///
/// Lifted out of `SettingsApp.svelte` in #129 (c): four call sites in three
/// sections use it (an executable for an external tool, a tool-plugin binary,
/// an avatar image, a transition video), and once those sections became
/// children the alternative was drilling a dialog-opener down as a prop.
///
/// A cancelled dialog and a failed one both answer `null`, so every caller's
/// "leave the current value alone" branch is the same one.
export async function pickFile(name: string, extensions: string[]): Promise<string | null> {
  try {
    const r = await open({ multiple: false, filters: [{ name, extensions }] });
    if (typeof r === 'string') return r;
    return null;
  } catch (e) {
    console.error('dialog open failed', e);
    return null;
  }
}

/// The filter every "point cImp at a tool" browse uses. `.cmd`/`.bat` are
/// included because many tools ship as launcher shims (npm bins, PMD's
/// pmd.bat) rather than real `.exe`s — the spawn path runs them through
/// cmd.exe, so they work anywhere an `.exe` does.
export const EXECUTABLE_EXTENSIONS = ['exe', 'cmd', 'bat', 'com'];

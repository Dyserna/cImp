// Dev-only harness entry for checks-harness.html (see that file's header).
import { mount } from 'svelte';
import ChecksEditor from '../lib/settings/ChecksEditor.svelte';
import type { CheckDef } from '../lib/settings/types';
import '../theme.css';
import '../app.css';
// The real app injects the active theme's CSS from disk at runtime; the
// harness imports one TUI theme directly to reproduce its structural
// button overrides (`section button` bracket framing).
import '../../themes/tui-blue/theme.css';

// `?theme=modern` renders with the base `:root` tokens only (no TUI
// structural overrides); default exercises the tui-blue bracket rules.
document.documentElement.dataset.theme =
  new URLSearchParams(location.search).get('theme') === 'modern' ? 'modern-dark' : 'tui-blue';

const checks: CheckDef[] = [
  {
    name: 'cargo-check',
    cmd: 'cargo check --message-format=json',
    parser: 'cargo-json',
    timeout_secs: 300,
    changed_only_arg: null,
    cwd: 'src-tauri',
    env: [['RUSTFLAGS', '-Dwarnings']],
    report_file: null,
    pattern: null,
    auto: true,
  } as unknown as CheckDef,
  {
    name: 'tsc',
    cmd: 'npx tsc --noEmit',
    parser: 'tsc',
    timeout_secs: 120,
    changed_only_arg: null,
    cwd: null,
    env: [],
    report_file: null,
    pattern: null,
    auto: false,
  } as unknown as CheckDef,
];

mount(ChecksEditor, {
  target: document.getElementById('harness')!,
  props: {
    checks,
    onchange: (next: CheckDef[]) => console.log('onchange', next),
  },
});

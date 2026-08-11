import { describe, expect, test } from 'vitest';

import {
  LOCAL_DATA_TOOLS,
  defaultSettings,
  localDataExcludedScope,
  toolScopeMode,
} from './types';

// #48, findings F-12 / F-27. `LOCAL_DATA_TOOLS` is hand-mirrored from Rust
// (`src-tauri/src/settings/schema.rs`) with no compile-time link, and the
// Settings window WRITES this list into a backend's `tool_scope`. These tests
// cannot see Rust — a Rust-side `include_str!` tripwire over `types.ts` is owed
// for that — but they do pin the two properties that made the stale mirror
// harmful: what the set must contain, and that the preset is identified by set
// membership rather than by array length.
describe('LOCAL_DATA_TOOLS (Rust mirror)', () => {
  test('run_check is a member — F-12: it executes the project’s configured commands', () => {
    expect(LOCAL_DATA_TOOLS).toContain('run_check');
  });

  test('the local-data set is exactly the seven Rust names', () => {
    expect([...LOCAL_DATA_TOOLS].sort()).toEqual(
      ['code_search', 'filesystem', 'git', 'list_dir', 'read_file', 'run_check', 'run_command'].sort(),
    );
  });

  test('no duplicates — the writer materializes this list verbatim', () => {
    expect(new Set(LOCAL_DATA_TOOLS).size).toBe(LOCAL_DATA_TOOLS.length);
  });
});

describe('toolScopeMode', () => {
  test('the scope the picker writes is the scope it recognizes', () => {
    expect(toolScopeMode(localDataExcludedScope())).toBe('web');
  });

  test('unrestricted is "all"', () => {
    expect(toolScopeMode({ mode: 'all' })).toBe('all');
  });

  test('an allow-list is always custom, even if it names the same tools', () => {
    expect(toolScopeMode({ mode: 'only', tools: [...LOCAL_DATA_TOOLS] })).toBe('custom');
  });

  // The F-27 regression: identification must not depend on array length.
  test('order and duplicates do not change what a scope means', () => {
    const shuffled = [...LOCAL_DATA_TOOLS].reverse();
    expect(toolScopeMode({ mode: 'allexcept', tools: shuffled })).toBe('web');
    expect(
      toolScopeMode({ mode: 'allexcept', tools: [...LOCAL_DATA_TOOLS, 'git', 'read_file'] }),
    ).toBe('web');
  });

  test('a stale exclusion missing run_check is NOT the preset', () => {
    // Exactly what a pre-F-12 install (or the old six-entry mirror) writes. It
    // is weaker than the preset, so calling it "web/docs only" would be the lie
    // F-27 named.
    const stale = LOCAL_DATA_TOOLS.filter((t) => t !== 'run_check');
    expect(toolScopeMode({ mode: 'allexcept', tools: stale })).toBe('custom');
  });

  test('the preset plus an extra exclusion is custom, not the preset', () => {
    expect(
      toolScopeMode({ mode: 'allexcept', tools: [...LOCAL_DATA_TOOLS, 'duckduckgo'] }),
    ).toBe('custom');
  });

  test('an empty exclusion list is not the preset (empty is not absent)', () => {
    expect(toolScopeMode({ mode: 'allexcept', tools: [] })).toBe('custom');
  });
});

describe('checks_allow_remote_worker', () => {
  test('F-12: denied by default', () => {
    expect(defaultSettings().checks_allow_remote_worker).toBe(false);
  });
});

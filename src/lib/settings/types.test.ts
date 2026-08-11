import { describe, expect, test } from 'vitest';

import {
  LOCAL_DATA_TOOLS,
  SPAWN_BAKED_INJECTION_FEATURES,
  defaultSettings,
  localDataExcludedScope,
  spawnBakedInjectionL2,
  spawnBakedTabOverrides,
  toolScopeMode,
} from './types';
import type { OffloadSettings, TabInjectionOverrides } from './types';

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

// #48, finding F-27's SECOND instance. The Settings window hand-mirrored Rust's
// `Feature::spawn_baked` in two places — a tab's L3 cells and the app-wide L2
// cells — and both went stale when `spotlighting` joined the set (M-3), so a
// Spotlighting flip raised no in-window restart hint. There is now ONE list and
// a `Record` over it, so the two readers cannot disagree with each other; these
// tests pin what the list must contain and that each member reads a DISTINCT,
// defined cell. What they still cannot see is Rust — the owed `include_str!`
// tripwire is the half that catches Rust growing a fifth member.
describe('SPAWN_BAKED_INJECTION_FEATURES (Rust mirror)', () => {
  test('spotlighting is a member — M-3: its launch addendum is baked', () => {
    expect(SPAWN_BAKED_INJECTION_FEATURES).toContain('spotlighting');
  });

  test('the spawn-baked set is exactly the four Rust features', () => {
    expect([...SPAWN_BAKED_INJECTION_FEATURES].sort()).toEqual(
      ['consumer_hygiene', 'native_web', 'opencode_native_gate', 'spotlighting'].sort(),
    );
  });

  test('no duplicates — both restart shapes materialize this list verbatim', () => {
    expect(new Set(SPAWN_BAKED_INJECTION_FEATURES).size).toBe(
      SPAWN_BAKED_INJECTION_FEATURES.length,
    );
  });
});

describe('spawnBakedInjectionL2', () => {
  const offload = (): OffloadSettings => defaultSettings().offload;

  test('every feature contributes a defined cell, one per member', () => {
    const cells = spawnBakedInjectionL2(offload());
    expect(cells).toHaveLength(SPAWN_BAKED_INJECTION_FEATURES.length);
    // A cell wired to a field that does not exist reads `undefined` and then
    // JSON.stringify drops it — the shape would silently stop tracking it.
    expect(cells.some((c) => c === undefined)).toBe(false);
  });

  // The property the two hand-lists could not hold: EVERY spawn-baked feature's
  // app-wide flip has to move the shape. Two accessors pointed at the same field
  // (or at the wrong one) would pass a length check and fail this.
  test('flipping any one feature moves the shape', () => {
    const flip: Record<string, (o: OffloadSettings) => void> = {
      spotlighting: (o) => (o.injection.spotlighting_enabled = !o.injection.spotlighting_enabled),
      // Not a boolean: `sensor` → `deny` keeps the feature ON and still changes
      // how a tab launches.
      native_web: (o) => (o.native_web_visibility = 'deny'),
      consumer_hygiene: (o) =>
        (o.injection.consumer_hygiene_enabled = !o.injection.consumer_hygiene_enabled),
      opencode_native_gate: (o) =>
        (o.injection.opencode_native_gate_enabled = !o.injection.opencode_native_gate_enabled),
    };
    const base = JSON.stringify(spawnBakedInjectionL2(offload()));
    for (const f of SPAWN_BAKED_INJECTION_FEATURES) {
      const o = offload();
      expect(flip[f], `no flip written for ${f}`).toBeTypeOf('function');
      flip[f](o);
      expect(JSON.stringify(spawnBakedInjectionL2(o)), f).not.toBe(base);
    }
  });

  test('native-web rides as its tri-mode, not as a boolean', () => {
    const sensor = offload();
    sensor.native_web_visibility = 'sensor';
    const deny = offload();
    deny.native_web_visibility = 'deny';
    expect(spawnBakedInjectionL2(sensor)).not.toEqual(spawnBakedInjectionL2(deny));
  });
});

describe('spawnBakedTabOverrides', () => {
  test('a tab with no overrides row reads as all-inherit, not as a change', () => {
    expect(spawnBakedTabOverrides(undefined)).toEqual(
      SPAWN_BAKED_INJECTION_FEATURES.map(() => 'inherit'),
    );
    expect(spawnBakedTabOverrides({})).toEqual(spawnBakedTabOverrides(undefined));
  });

  test('it reports the spawn-baked overrides and ignores the live ones', () => {
    const overrides: Partial<TabInjectionOverrides> = {
      spotlighting: 'off',
      // Live features must not reach a restart hint — a nag for a change that
      // takes effect immediately is how a hint stops being read.
      taint_latch: 'off',
      detection: 'off',
    };
    const got = spawnBakedTabOverrides(overrides);
    expect(got).toEqual(spawnBakedTabOverrides({ spotlighting: 'off' }));
    expect(got[SPAWN_BAKED_INJECTION_FEATURES.indexOf('spotlighting')]).toBe('off');
    expect(got.filter((v) => v === 'off')).toHaveLength(1);
  });

  // M-3, as the Settings window saw it: this call is the whole difference
  // between "flipping Spotlighting on a tab raises a restart hint" and silence.
  test('a spotlighting override alone changes the tab shape', () => {
    expect(spawnBakedTabOverrides({ spotlighting: 'on' })).not.toEqual(
      spawnBakedTabOverrides({}),
    );
  });
});

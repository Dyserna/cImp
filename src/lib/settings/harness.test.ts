import { describe, expect, test } from 'vitest';

import { defaultSettings, harnessRow, setHarnessExt } from './types';

// V40 Phase B (locked decision 5). The frontend half of the per-harness map has
// exactly one property that matters and it is easy to get wrong: **an absent
// row is the ORDINARY case**, not an error state. A fresh install that has
// never saved, a settings file written before the harness registered, a
// harness added in the build the user just upgraded to — all three produce a
// `Settings.harness` with no key for it, and the backend answers the declared
// defaults for every one. A window that rendered `undefined` into a checkbox
// would show every per-harness switch OFF for a harness whose real state is ON.
describe('harnessRow', () => {
  test('an absent row reads the same defaults the backend resolves', () => {
    const s = defaultSettings();
    s.harness = {};
    const row = harnessRow(s, 'claude');
    expect(row.expose_commands).toBe(true);
    expect(row.expose_code_audit).toBe(true);
    expect(row.input_profile_status).toBe('unverified');
    expect(row.last_seen).toBe('');
    expect(row.ext).toEqual({});
  });

  test('a harness this build has never heard of reads the same defaults', () => {
    // Not a hypothetical: the map is keyed by string precisely so a row written
    // by a newer build survives, and the window must render such a row rather
    // than crashing on it.
    const s = defaultSettings();
    expect(harnessRow(s, 'codex').expose_commands).toBe(true);
  });

  test('a present row is returned as it is', () => {
    const s = defaultSettings();
    s.harness = {
      claude: {
        expose_commands: false,
        expose_code_audit: false,
        last_seen: '2.2.0',
        last_verified: '2.1.0',
        input_profile_status: 'pass',
        ext: { statusline: false },
      },
    };
    expect(harnessRow(s, 'claude').last_seen).toBe('2.2.0');
    expect(harnessRow(s, 'claude').ext['statusline']).toBe(false);
  });
});

describe('setHarnessExt', () => {
  test('writes into a row that does not exist yet, without inventing others', () => {
    const s = defaultSettings();
    s.harness = {};
    setHarnessExt(s, 'opencode', 'native_gate', false);
    expect(s.harness['opencode'].ext['native_gate']).toBe(false);
    expect(Object.keys(s.harness)).toEqual(['opencode']);
    // The core fields of the created row are the declared defaults, so a write
    // to `ext` cannot silently turn an exposure switch off.
    expect(s.harness['opencode'].expose_commands).toBe(true);
  });

  test('leaves the other keys on the row alone', () => {
    const s = defaultSettings();
    setHarnessExt(s, 'claude', 'statusline', false);
    setHarnessExt(s, 'claude', 'local.base_url', 'http://elsewhere:1');
    expect(s.harness['claude'].ext).toEqual({
      statusline: false,
      'local.base_url': 'http://elsewhere:1',
    });
  });

  test('does not touch another harness', () => {
    const s = defaultSettings();
    setHarnessExt(s, 'claude', 'statusline', false);
    setHarnessExt(s, 'opencode', 'provider_auto', true);
    expect(s.harness['claude'].ext['provider_auto']).toBeUndefined();
    expect(s.harness['opencode'].ext['statusline']).toBeUndefined();
  });
});

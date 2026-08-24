import { describe, expect, test } from 'vitest';

import { defaultSettings, harnessRow, setHarnessExt } from './types';

import { FIRST_HARNESS, SECOND_HARNESS } from '../harness.fixture';

// V40 Phase F: the two harness keys come from the committed registry fixture,
// so this suite is about the MAP's behaviour and not about which products are
// in it.
const H1 = FIRST_HARNESS.id;
const H2 = SECOND_HARNESS.id;

// V40 review finding L-17: the ext KEYS come from the fixture too, not just the
// harness ids. They were hand-typed (`'statusline'`, `'native_gate'`,
// `'local.base_url'`, `'provider_auto'`), so renaming a declared field key in
// Rust left this suite green while every assertion in it was about a key
// nothing declares — which is the same class of "a test that stopped asserting
// what it used to assert" the rest of Phase G went through.
function keyOf(h: typeof FIRST_HARNESS, i: number): string {
  const key = h.fields[i]?.key;
  if (!key) throw new Error(`${h.id} declares no field #${i} — re-point this suite`);
  return key;
}
const H1_KEY_A = keyOf(FIRST_HARNESS, 0);
const H1_KEY_B = keyOf(FIRST_HARNESS, 1);
const H2_KEY_A = keyOf(SECOND_HARNESS, 0);
const H2_KEY_B = keyOf(SECOND_HARNESS, 1);

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
    const row = harnessRow(s, H1);
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
      [H1]: {
        expose_commands: false,
        expose_code_audit: false,
        last_seen: '2.2.0',
        last_verified: '2.1.0',
        input_profile_status: 'pass',
        // Required since V42 Phase E: the generated `HarnessSettings` spells
        // Rust's `Option<AutoVerify>` as `AutoVerify | null`, not `?`.
        auto_verify: null,
        ext: { [H1_KEY_A]: false },
      },
    };
    expect(harnessRow(s, H1).last_seen).toBe('2.2.0');
    expect(harnessRow(s, H1).ext[H1_KEY_A]).toBe(false);
  });
});

describe('setHarnessExt', () => {
  test('writes into a row that does not exist yet, without inventing others', () => {
    const s = defaultSettings();
    s.harness = {};
    setHarnessExt(s, H2, H2_KEY_A, false);
    expect(s.harness[H2].ext[H2_KEY_A]).toBe(false);
    expect(Object.keys(s.harness)).toEqual([H2]);
    // The core fields of the created row are the declared defaults, so a write
    // to `ext` cannot silently turn an exposure switch off.
    expect(s.harness[H2].expose_commands).toBe(true);
  });

  test('leaves the other keys on the row alone', () => {
    const s = defaultSettings();
    setHarnessExt(s, H1, H1_KEY_A, false);
    setHarnessExt(s, H1, H1_KEY_B, 'http://elsewhere:1');
    expect(s.harness[H1].ext).toEqual({
      [H1_KEY_A]: false,
      [H1_KEY_B]: 'http://elsewhere:1',
    });
  });

  test('does not touch another harness', () => {
    const s = defaultSettings();
    setHarnessExt(s, H1, H1_KEY_A, false);
    setHarnessExt(s, H2, H2_KEY_B, true);
    expect(s.harness[H1].ext[H2_KEY_B]).toBeUndefined();
    expect(s.harness[H2].ext[H1_KEY_A]).toBeUndefined();
  });
});

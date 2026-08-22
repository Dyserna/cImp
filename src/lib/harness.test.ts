import { describe, expect, test } from 'vitest';

import registry from '../../src-tauri/fixtures/harness/registry.json';
import {
  HARNESS_FEATURES,
  accentFor,
  defaultTabId,
  findHarness,
  findHarnessByCommand,
  findHarnessByTabId,
  harnessLabels,
  harnessLabelsProse,
  harnessesWith,
  labelForHarness,
  labelForTabId,
  renderAttribution,
  reservedAiTabIds,
  scopedFeatureOwner,
  type HarnessInfo,
} from './harness';
import { FIXTURE_HARNESSES } from './harness.fixture';
import { tabHarness } from './delegation';
import { harnesses } from './harness';

/// V40 Phase F — **the frontend half of the registry parity check** (locked
/// decision 11).
///
/// `src-tauri/fixtures/harness/registry.json` is written by the Rust test
/// `harness::info::tests::the_committed_registry_fixture_matches_the_registry`,
/// which fails `cargo test` when the file on disk differs from `HARNESSES`. So
/// the file IS the registry, and everything below is a statement about the
/// registry that `vitest` can make: the TypeScript unions cover what it
/// carries, this module's declared shapes cover its fields, and the frontend's
/// two "which harness is this?" answers agree with every descriptor.
///
/// A descriptor field, feature or harness added in Rust without its TypeScript
/// mirror is a red `npm test` rather than a runtime `undefined`.

/// The fixture as raw JSON, so the key-coverage tests below can see keys the
/// TypeScript interface does not declare — which is exactly the drift they are
/// looking for.
const RAW = registry as unknown as Record<string, unknown>[];

/// The keys `HarnessInfo` declares. Spelled as data because TypeScript types
/// are erased at runtime: this list and the interface are checked against each
/// other by the compiler (the object below must type-check), and against the
/// fixture by the test.
const INFO_KEYS: (keyof HarnessInfo)[] = [
  'id',
  'label',
  'tab_ids',
  'binaries',
  'features',
  'consumer',
  'affordances',
  'fields',
  'scoped_features',
];

/// The keys `HarnessAffordances` declares (locked decision 27's list).
const AFFORDANCE_KEYS = [
  'newSessionCommand',
  'toolListRefresh',
  'webTools',
  'stateDirs',
  'installHint',
  'docsUrl',
  'attachmentFormat',
  'localProvider',
  'localProviderNote',
  'localProviderConfigNote',
  'statuslineRows',
  'attributionTemplate',
  'injectMechanism',
  'defaultCommand',
  'commandExample',
  'accent',
  'tier',
] as const;

describe('registry parity (locked decision 11)', () => {
  test('the fixture is non-empty and every harness declares its identity', () => {
    // A fixture that silently became `[]` would make every test below vacuous.
    expect(RAW.length).toBeGreaterThan(0);
    for (const h of FIXTURE_HARNESSES) {
      expect(h.id, 'a harness with no id').toBeTruthy();
      expect(h.label, `${h.id}: no label`).toBeTruthy();
      expect(h.tab_ids.length, `${h.id}: no reserved tab id`).toBeGreaterThan(0);
      expect(h.binaries.length, `${h.id}: no binary`).toBeGreaterThan(0);
      expect(h.consumer, `${h.id}: no consumer token`).toBeTruthy();
    }
  });

  test('the TypeScript HarnessFeature union covers every declared feature', () => {
    for (const h of RAW) {
      for (const f of h.features as string[]) {
        expect(
          HARNESS_FEATURES as readonly string[],
          `feature ${f} is declared in Rust but is not in the TypeScript union`,
        ).toContain(f);
      }
    }
  });

  test('no feature in the union is unknown to Rust', () => {
    // The other direction: a token left behind by a rename would make every
    // `features.includes(...)` on it silently false, so a panel would simply
    // never mount.
    const declared = new Set(RAW.flatMap((h) => h.features as string[]));
    for (const f of HARNESS_FEATURES) {
      expect(declared, `the TypeScript union has ${f}, which no harness declares`).toContain(f);
    }
  });

  test('HarnessInfo declares exactly the fields the payload carries', () => {
    for (const h of RAW) {
      expect(Object.keys(h).sort()).toEqual([...INFO_KEYS].sort());
    }
  });

  test('HarnessAffordances declares exactly the affordances the payload carries', () => {
    for (const h of RAW) {
      const a = h.affordances as Record<string, unknown>;
      expect(Object.keys(a).sort()).toEqual([...AFFORDANCE_KEYS].sort());
    }
  });

  test('every affordance the window renders verbatim is a string or absent', () => {
    for (const h of FIXTURE_HARNESSES) {
      const a = h.affordances;
      expect(typeof a.attachmentFormat).toBe('string');
      expect(a.attachmentFormat).toContain('{path}');
      expect(a.attributionTemplate).toContain('{label}');
      expect(a.attributionTemplate).toContain('{tab}');
      expect(typeof a.statuslineRows).toBe('number');
      expect(typeof a.accent).toBe('string');
      expect(typeof a.tier).toBe('string');
    }
  });
});

describe('the lookups agree with every descriptor', () => {
  test('tabHarness classifies each harness by its declared binaries', () => {
    // Amendment 0-d: the frontend's SECOND "which harness" function, pinned
    // against the registry. `tabHarness` and `findHarnessByCommand` must give
    // the same answer the Rust `tab_consumer` gives, or the delegation popover
    // names a tab the backend would not have displaced.
    harnesses.set(FIXTURE_HARNESSES);
    for (const h of FIXTURE_HARNESSES) {
      for (const bin of h.binaries) {
        for (const command of [bin, `${bin}.exe`, `/usr/bin/${bin}`, `C:/bin/${bin.toUpperCase()}.CMD`]) {
          expect(findHarnessByCommand(FIXTURE_HARNESSES, command)?.id, command).toBe(h.id);
          expect(
            tabHarness({ command } as Parameters<typeof tabHarness>[0]),
            command,
          ).toBe(h.id);
        }
      }
    }
  });

  test('a command no harness declares is nobody, not the first one', () => {
    harnesses.set(FIXTURE_HARNESSES);
    for (const command of ['', 'bash', 'pwsh', 'a-harness-from-the-future']) {
      expect(findHarnessByCommand(FIXTURE_HARNESSES, command)).toBeNull();
      expect(tabHarness({ command } as Parameters<typeof tabHarness>[0])).toBeNull();
    }
  });

  test('every reserved tab id resolves to exactly one harness', () => {
    const seen = new Set<string>();
    for (const id of reservedAiTabIds(FIXTURE_HARNESSES)) {
      expect(seen.has(id), `${id} is claimed by two harnesses`).toBe(false);
      seen.add(id);
      expect(findHarnessByTabId(FIXTURE_HARNESSES, id)).not.toBeNull();
    }
    expect(findHarnessByTabId(FIXTURE_HARNESSES, 'ai-1234')).toBeNull();
    expect(findHarnessByTabId(FIXTURE_HARNESSES, 'shell-default-1')).toBeNull();
  });

  test('the bootstrap fallback equals the registry, in both directions', () => {
    // `harness.ts`'s ONE declared identity: the synchronous reserved-tab list
    // locked decision 7 sanctions, read only until `harness_list` answers.
    // Static data cannot disagree with the registry — but only while something
    // checks, which is this.
    const bootstrap = reservedAiTabIds([]);
    expect(bootstrap).toEqual(reservedAiTabIds(FIXTURE_HARNESSES));
  });

  test('the default tab is the first reserved tab of the first harness', () => {
    expect(defaultTabId(FIXTURE_HARNESSES)).toBe(FIXTURE_HARNESSES[0].tab_ids[0]);
    // Before the IPC answers, the bootstrap list still gives a real tab id.
    expect(defaultTabId([])).toBe(FIXTURE_HARNESSES[0].tab_ids[0]);
  });

  test('labels come from the descriptor, and an unknown id renders as itself', () => {
    for (const h of FIXTURE_HARNESSES) {
      expect(labelForHarness(FIXTURE_HARNESSES, h.id)).toBe(h.label);
      expect(labelForTabId(FIXTURE_HARNESSES, h.tab_ids[0])).toBe(h.label);
      for (const id of h.tab_ids.slice(1)) {
        // A further reserved tab is a VARIANT: the harness's label plus the
        // suffix already in the tab id.
        expect(labelForTabId(FIXTURE_HARNESSES, id)).toBe(
          `${h.label} (${id.slice(h.id.length + 1)})`,
        );
      }
    }
    expect(labelForHarness(FIXTURE_HARNESSES, 'nobody')).toBe('nobody');
    expect(labelForHarness(FIXTURE_HARNESSES, '')).toBe('another harness');
    expect(labelForTabId(FIXTURE_HARNESSES, 'shell-default-1')).toBe('');
  });

  test('the roster renders as copy, in registry order', () => {
    const labels = FIXTURE_HARNESSES.map((h) => h.label);
    expect(harnessLabels(FIXTURE_HARNESSES)).toBe(labels.join(' / '));
    expect(harnessLabelsProse(FIXTURE_HARNESSES)).toContain(labels[labels.length - 1]);
    expect(harnessLabels([])).toBe('');
    expect(harnessLabelsProse([])).toBe('');
  });

  test('accents are per harness, and an unknown harness gets none', () => {
    for (const h of FIXTURE_HARNESSES) {
      expect(accentFor(FIXTURE_HARNESSES, h.id)).toBe(h.affordances.accent);
    }
    expect(accentFor(FIXTURE_HARNESSES, 'nobody')).toBe('');
    expect(accentFor(FIXTURE_HARNESSES, null)).toBe('');
  });

  test('the attribution line comes from the driver harness template', () => {
    for (const h of FIXTURE_HARNESSES) {
      expect(renderAttribution(FIXTURE_HARNESSES, h.id, 'api-work')).toBe(
        h.affordances.attributionTemplate
          .replace('{label}', h.label)
          .replace('{tab}', 'api-work'),
      );
    }
    // A driver this build does not know still renders a COMPLETE line.
    const unknown = renderAttribution(FIXTURE_HARNESSES, 'from-the-future', null);
    expect(unknown).toContain('from-the-future');
    expect(unknown).toContain('another tab');
  });

  test('a scoped feature names a harness and a key that harness declares', () => {
    for (const h of FIXTURE_HARNESSES) {
      for (const f of h.scoped_features) {
        const owner = scopedFeatureOwner(FIXTURE_HARNESSES, f.feature);
        expect(owner?.harness.id).toBe(h.id);
        expect(owner?.extKey).toBe(f.extKey);
        expect(
          h.fields.map((x) => x.key),
          `${h.id}: scoped feature ${f.feature} reads ${f.extKey}, which it does not declare`,
        ).toContain(f.extKey);
      }
    }
    expect(scopedFeatureOwner(FIXTURE_HARNESSES, 'nothing.scopes.this')).toBeNull();
  });

  test('findHarness and harnessesWith are lookups, never guesses', () => {
    for (const h of FIXTURE_HARNESSES) {
      expect(findHarness(FIXTURE_HARNESSES, h.id)?.id).toBe(h.id);
      for (const f of h.features) {
        expect(harnessesWith(FIXTURE_HARNESSES, f).map((x) => x.id)).toContain(h.id);
      }
    }
    expect(findHarness(FIXTURE_HARNESSES, 'nobody')).toBeNull();
    expect(findHarness(FIXTURE_HARNESSES, null)).toBeNull();
    expect(findHarness([], FIXTURE_HARNESSES[0].id)).toBeNull();
  });
});

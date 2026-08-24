import { describe, expect, test } from 'vitest';

import { FIXTURE_HARNESSES } from './harness.fixture';

/// V40 Phase F — **the frontend's identity allowlist**, the mirror of Rust's
/// `harness::layering::no_harness_identity_outside_registry` (locked decisions
/// 7, 10(a) and 27).
///
/// The phase's acceptance criterion is that a harness's id, binary, reserved
/// tab id or product name appears in `src/` only where a documented row below
/// says it may. A grep is not that criterion: a grep is something a person
/// runs once. This is, and it is checked in BOTH directions — a row that no
/// longer matches anything fails too, so the list cannot quietly accumulate
/// exemptions for code that has already been fixed.
///
/// What it scans for: every registry id, reserved tab id, binary, consumer
/// token and LABEL, case-insensitively, as a whole word. Case-insensitively
/// because the offence this phase removes is as often PROSE — a sentence in the
/// Settings window telling the user to restart a named product's tab — as it is
/// a comparison, and that prose is exactly what locked decision 27 moved into
/// the affordances.

/// One exempt file, with the reason it is exempt. The same discipline as the
/// Rust `IDENTITY_ALLOWLIST`: a row is a decision on the record, not a
/// convenience.
const ALLOWLIST: { file: string; reason: string }[] = [
  {
    file: 'lib/harness.ts',
    reason:
      'THE registry mirror. Its one declared identity is BOOTSTRAP_RESERVED_TAB_IDS — locked ' +
      'decision 7\'s sanctioned synchronous fallback, read only until `harness_list` answers ' +
      'and asserted equal to the registry in both directions by `harness.test.ts`.',
  },
  {
    file: 'lib/settings/types.ts',
    reason:
      'PERSISTED WIRE FORMS, and after Phase F that is all that is left: ' +
      '`HARNESS_NATIVE_GATE_KEY` is a `TabInjectionOverrides` field in every settings file on ' +
      'disk and a `/status` row name, so renaming it is a migration. Which harness owns that ' +
      'feature is `harness_list`\'s `scoped_features` — nothing here reads the key as an ' +
      'identity. Same rule Rust applies to `settings/schema.rs`.',
  },
  {
    file: 'lib/settings/generated/settings.ts',
    reason:
      'GENERATED, not written: V42 Phase E emits it from `src-tauri/src/settings/schema.rs` via ' +
      'ts-rs, doc comments included. Every name it carries is one the Rust IDENTITY_ALLOWLIST ' +
      'already rules on for that file (persisted wire forms, plus the prose beside them); nothing ' +
      'here is a second declaration, and nothing here is rendered to a user. Editing the file is ' +
      'pointless — CI regenerates it and diffs — so the fix for a name that should not be in it ' +
      'is to change the Rust doc comment.',
  },
  {
    file: 'lib/avatarConfig.ts',
    reason:
      'A BRAND ASSET, ruled so by locked decision 29: a mascot sprite set depends on no ' +
      'harness, cImp ships it as images under `sprites/`, and renaming the folder would be a ' +
      'settings migration for no gain. `SPRITE_SETS` is the one place it is named; the ' +
      'Settings picker iterates it.',
  },
  {
    file: 'lib/themes/index.ts',
    reason:
      'A PERSISTED PALETTE NAME, ruled so by locked decision 29 for the same reason as the ' +
      'sprite set: the palette is a colour table named after a product, written into every ' +
      'settings file, and depends on no harness. `DEFAULT_PALETTE_NAME` is the one spelling.',
  },
  {
    file: 'lib/themes/registry.ts',
    reason:
      'The bundled themes\' `palette` metadata — the pairing that names the default palette ' +
      'above. Same persisted-name exemption.',
  },
];

/// Every frontend source file, as text.
///
/// Read through Vite's own glob rather than through `node:fs`: the suite must
/// run under the same module resolution the app is built with, and the project
/// deliberately carries no Node type declarations. Keys are project-relative
/// (`/src/lib/foo.ts`).
const SOURCES = import.meta.glob('/src/**/*.{ts,svelte}', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/// A key of [`SOURCES`], as the allowlist spells it (`lib/foo.ts`).
function relative(key: string): string {
  return key.replace(/^\/src\//, '');
}

/// Every identity string the registry publishes: ids, reserved tab ids,
/// binaries, consumer tokens and display labels.
function identityTerms(): string[] {
  const terms = new Set<string>();
  for (const h of FIXTURE_HARNESSES) {
    terms.add(h.id);
    terms.add(h.consumer);
    terms.add(h.label);
    for (const t of h.tab_ids) terms.add(t);
    for (const b of h.binaries) terms.add(b);
    // The label is added WHOLE and never split into words: a product name's
    // trailing word is usually a common one, and a term like "Code" would
    // flag half the tree. Its leading word is normally the id, which is
    // already a term — so prose naming the product is caught by the id,
    // case-insensitively.
  }
  return [...terms].sort((a, b) => b.length - a.length);
}

function hits(text: string, terms: string[]): string[] {
  const found: string[] = [];
  for (const term of terms) {
    // Whole word, case-insensitive. A hyphenated reserved tab id counts, a
    // dotfile path built from a harness id counts, and an unrelated word that
    // merely contains the letters does not.
    //
    // **…and a camelCase IDENTIFIER counts** (V40 review finding L-16). The
    // boundary used to be `[^A-Za-z0-9]` on both sides, which is exactly the
    // spelling the identifiers develop actually had — `getClaudeUsage`,
    // `claudePushTabActive`, `isClaudeTab` — so the tripwire would have passed
    // on the pre-V40 tree it was written to police. A term is also a hit when
    // it sits at a case boundary: preceded by a lowercase letter or digit and
    // starting uppercase (`getClaudeUsage`), or followed by an uppercase letter
    // (`claudePushTab`). One helper name away from decorative, before this.
    const t = escapeRe(term);
    const patterns = [
      // Whole word.
      new RegExp(`(^|[^A-Za-z0-9])${t}([^A-Za-z0-9]|$)`, 'i'),
      // `…somethingClaude…` — a lowercase/digit run, then the term capitalised.
      new RegExp(`[a-z0-9]${t}([^a-z0-9]|$)`),
      // `…claudeSomething` — the term, then an uppercase letter.
      new RegExp(`(^|[^A-Za-z0-9])${t}[A-Z]`, 'i'),
    ];
    if (patterns.some((re) => re.test(text))) found.push(term);
  }
  return found;
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

describe('no harness identity outside the registry (locked decision 10(a), frontend)', () => {
  const terms = identityTerms();
  const allowed = new Set(ALLOWLIST.map((r) => r.file));

  test('the scan has something to scan for', () => {
    // A registry fixture that became empty — or a glob that matched nothing —
    // would make every assertion below vacuously true, which is the classic way
    // a tripwire stops being one.
    expect(terms.length).toBeGreaterThan(0);
    expect(Object.keys(SOURCES).length).toBeGreaterThan(50);
  });

  test('no source file outside the allowlist names a harness', () => {
    const offenders: Record<string, string[]> = {};
    for (const [key, text] of Object.entries(SOURCES)) {
      const rel = relative(key);
      if (allowed.has(rel)) continue;
      const found = hits(text, terms);
      if (found.length > 0) offenders[rel] = found;
    }
    expect(
      offenders,
      'these files name a harness. Ask `harness_list` (src/lib/harness.ts) for the id, the ' +
        'label or the affordance instead — or add an ALLOWLIST row here with the reason, the ' +
        'way Rust\'s IDENTITY_ALLOWLIST does.',
    ).toEqual({});
  });

  test('every allowlist row still matches a real file that still needs it', () => {
    // The other direction. A row for a file that has been fixed, moved or
    // deleted is an exemption nobody is using — and the next file to need one
    // would inherit it silently.
    for (const row of ALLOWLIST) {
      const text = SOURCES[`/src/${row.file}`];
      expect(text, `${row.file} is on the allowlist but does not exist`).toBeTypeOf('string');
      const found = hits(text ?? '', terms);
      expect(
        found.length,
        `${row.file} is on the allowlist but no longer names a harness — remove the row`,
      ).toBeGreaterThan(0);
      expect(row.reason.length, `${row.file}: an allowlist row needs a reason`).toBeGreaterThan(40);
    }
  });
});

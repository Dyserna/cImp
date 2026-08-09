import { describe, it, expect } from 'vitest';

/// #48, M-25 — the detection-status contract, ENFORCED rather than documented.
///
/// `offload.ts` already carried the rule: `healthy` is the field that answers
/// "is the signature layer protecting this app?", computed in Rust and "never
/// restated". `latch.ts` lifted `rules.armed` out of the same object anyway —
/// the weaker predicate, "can this match ANYTHING at all?" — so a rules
/// directory of four files with three failing to compile rendered as FULL
/// protection on the status chip, the tab badges and the taint popover.
///
/// The lesson this milestone keeps re-learning is that a documented contract
/// with no mechanical enforcement gets violated again, usually by someone who
/// never read the comment. This test is the enforcement: exactly two files may
/// touch `rules.armed` / `rules.healthy`, and both are listed below with the
/// reason they may.
///
/// **Adding a file here is a review decision, not a formality.** A consumer
/// that wants to know whether protection is intact wants `rulesHealth()` and
/// `healthy`, and does not belong on this list.

/// Every shipping frontend source, as text. `.test.ts` files are excluded on
/// purpose: a test constructs these states deliberately (this one names them in
/// its own prose), and no test renders anything to a user.
///
/// Read through Vite's own glob rather than `node:fs` — the app's tsconfig has
/// no node types, and a test that cannot be type-checked is a poor guardian of
/// a type-level contract. The third case below fails loudly if this ever
/// resolves to the wrong tree, which is the failure mode of every source scan.
const SOURCES = import.meta.glob(
  ['/src/**/*.ts', '/src/**/*.svelte', '!/src/**/*.test.ts'],
  { query: '?raw', import: 'default', eager: true },
) as Record<string, string>;

/// The two sanctioned readers, each with the reason it is one.
const ALLOWED = new Map<string, string>([
  [
    '/src/lib/offload.ts',
    'declares DetectionStatus and owns rulesHealth(), the single extractor every other consumer goes through',
  ],
  [
    '/src/SettingsApp.svelte',
    'the detection panel renders the raw read: `healthy` drives the dot (#48, N-3) and `armed` adds the "nothing to match with" clause beside the file counts',
  ],
]);

/// A read of either rule-set predicate off a detection status.
const PREDICATE = /\.rules\s*\.\s*(armed|healthy)\b/;

describe('the detection-status contract', () => {
  it('has no reader of rules.armed / rules.healthy outside the two sanctioned ones', () => {
    const offenders: string[] = [];
    for (const [path, text] of Object.entries(SOURCES)) {
      if (ALLOWED.has(path)) continue;
      text.split(/\r?\n/).forEach((line, i) => {
        if (PREDICATE.test(line)) offenders.push(`${path}:${i + 1}  ${line.trim()}`);
      });
    }
    expect(
      offenders,
      'Read the detection status through `rulesHealth()` (src/lib/offload.ts) and branch on ' +
        '`healthy`. `armed` is true of a rule set that lost most of its files, and a surface ' +
        'that renders it as protected tells the user they are covered when they are not ' +
        '(#48, M-25). If this file genuinely must show the raw read, add it to ALLOWED with ' +
        'the reason.',
    ).toEqual([]);
  });

  it('still finds both sanctioned readers where it expects them', () => {
    // An allowlist nobody prunes is how the next exemption gets granted by
    // accident: a file that stopped reading the raw status must stop being
    // exempt from reading it.
    for (const [path, why] of ALLOWED) {
      expect(SOURCES[path], `${path} is on the allowlist but not in the scan`).toBeTypeOf('string');
      expect(PREDICATE.test(SOURCES[path]), `${path} — ${why}`).toBe(true);
    }
  });

  it('scans the tree it thinks it is scanning', () => {
    // The whole test is a grep, so an empty or wrong-rooted scan would pass it
    // by finding nothing.
    const paths = Object.keys(SOURCES);
    expect(paths).toContain('/src/lib/latch.ts');
    expect(paths).toContain('/src/SettingsApp.svelte');
    expect(paths.length).toBeGreaterThan(20);
    // …and the exclusion holds, so a consumer cannot hide in a `.test.ts`.
    expect(paths.filter((p) => p.endsWith('.test.ts'))).toEqual([]);
  });
});

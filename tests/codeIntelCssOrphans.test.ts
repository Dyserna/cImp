// Guard: every class a Code Intelligence SECTION puts in its markup must have
// a rule somewhere that can actually reach it.
//
// Why this exists (#130, the CodeIntelligenceView split). Four sections were
// cut out of one 4,568-line component, markup and CSS together, after the
// rules more than one section needs were hoisted into `codeIntel.css`. Both
// halves of that can go wrong silently: a rule left behind in the parent's
// scoped `<style>` stops reaching the markup that moved, and a class whose
// only rule was supposed to be hoisted but wasn't has nothing to match it.
// Neither is a build error, and the page still renders — flat.
//
// Same failure mode, same scan, same file as #129's settings guard:
// `tests/cssOrphans.ts` holds the machinery (where a rule may live, how
// classes are read out of markup, why the matching is permissive). This file
// is the Code Intelligence split's inputs and its allowlist.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { scanSplit } from './cssOrphans';
import { rel, REPO_ROOT } from './repoFiles';

const CODE_INTEL_DIR = join(REPO_ROOT, 'src', 'lib', 'codeIntel');

/**
 * Classes that legitimately have no rule, each with the reason.
 *
 * A row is a claim that the class is not supposed to be styled; `the allowlist
 * has no stale rows` below fails if one ever acquires a rule, so the list
 * cannot quietly outlive its reasons. Adding a row is the wrong first move: for
 * a newly flagged class the usual right answer is that its rule stayed behind
 * in `CodeIntelligenceView.svelte`, or belongs in `codeIntel.css`.
 */
const NO_RULE_EXPECTED = new Map<string, string>([
  [
    'path-sec',
    'unstyled wrapper round the Trace path card — it groups the caveat with the card, and the card carries the box',
  ],
  ['arch-sec', 'unstyled wrapper round the Architecture cards, same as .path-sec'],
  [
    'costrows',
    'unstyled (PLURAL) wrapper round the per-model cost rows — the singular `.costrow` on its children is what carries the layout. Verified pre-existing: it had no rule in `CodeIntelligenceView.svelte` at `2fa98e2` either, so this is a latent typo-shaped class the split surfaced rather than caused. Fixing it would be a visual change and belongs in its own commit.',
  ],
]);

describe('code intelligence section CSS', () => {
  const { findings, children, classes, shared } = scanSplit({
    childrenDir: CODE_INTEL_DIR,
    parentFile: join(REPO_ROOT, 'src', 'lib', 'CodeIntelligenceView.svelte'),
    sharedSheets: [join(CODE_INTEL_DIR, 'codeIntel.css')],
  });

  test('the scan actually sees the sections (vacuity guard)', () => {
    // Four sections and ~200 class uses at the time of writing. The floors sit
    // deliberately below that: their job is to make "the walk found nothing"
    // and "the sheet did not load" fail HERE rather than read as a clean bill
    // of health, not to police the section count.
    expect(children, `${rel(CODE_INTEL_DIR)} yielded no components`).toBeGreaterThan(3);
    expect(classes, 'no classes were collected — the markup parse is broken').toBeGreaterThan(80);
    // The shared sheet must actually have been read: a typo in the path would
    // otherwise turn every hoisted class into a finding, and the allowlist
    // would grow to hide it. `.arow` is the most-hoisted family's anchor.
    expect(shared.has('arow'), 'codeIntel.css did not reach the scan').toBe(true);
    expect(shared.has('icon'), 'no injected theme sheet reached the scan').toBe(true);
  });

  test('no section uses a class whose rule stayed behind in CodeIntelligenceView (#130)', () => {
    // The extraction defect itself, and it is never benign: the allowlist does
    // NOT apply here. A hit means the markup moved into a child and the rule
    // did not follow — move the rule into that section's `<style>` block, or
    // into `codeIntel.css` if more than one section needs it.
    const orphans = findings
      .filter((f) => f.inParent)
      .map(
        (f) => `${f.child}  .${f.cls}  (rule is in src/lib/CodeIntelligenceView.svelte's block)`,
      );
    expect(
      orphans,
      "These classes are used in a section child while their only rule sits in the PARENT's " +
        'scoped <style>, so the rule matches nothing and the markup renders unstyled:\n' +
        orphans.join('\n'),
    ).toEqual([]);
  });

  test('every class a section uses has a rule that reaches it', () => {
    const unresolved = findings
      .filter((f) => !NO_RULE_EXPECTED.has(f.cls))
      .map((f) => `${f.child}  .${f.cls}`);
    expect(
      unresolved,
      'These classes have no rule in the section itself, in codeIntel.css / theme.css / app.css, ' +
        'in an injected theme sheet, or in any :global(...) — so they style nothing. Add the ' +
        'rule where the markup is, or (if the class is deliberately unstyled) add a row to ' +
        'NO_RULE_EXPECTED with the reason:\n' +
        unresolved.join('\n'),
    ).toEqual([]);
  });

  test('the allowlist has no stale rows', () => {
    const flagged = new Set(findings.map((f) => f.cls));
    const stale = [...NO_RULE_EXPECTED.keys()].filter((cls) => !flagged.has(cls));
    expect(
      stale,
      'These classes are allowlisted as deliberately unstyled but the scan no longer flags ' +
        'them — either a rule now exists (drop the row: the exemption is hiding a real rule ' +
        'from review) or the class is gone (drop the row):\n' +
        stale.join('\n'),
    ).toEqual([]);
    for (const [cls, reason] of NO_RULE_EXPECTED) {
      expect(reason, `.${cls} is allowlisted with no reason given`).not.toBe('');
    }
  });
});

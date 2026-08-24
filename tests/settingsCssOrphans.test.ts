// Guard: every class a Settings SECTION puts in its markup must have a rule
// somewhere that can actually reach it.
//
// Why this exists (#129, the SettingsApp split). Twenty-one sections were cut
// out of one 9,000-line component, markup and CSS together. Svelte scopes a
// component's `<style>` to that component's own markup, so a rule left behind
// in `SettingsApp.svelte` while its markup moved into a section child stops
// applying — silently, with a green build and a page that still renders.
//
// The extraction is done, so the value now is regression: the next section that
// moves, or the next rule someone relocates into `settings-chrome.css`, gets
// checked at test time instead of at "why is this row suddenly flat".
//
// The scan itself — where a rule may live, how classes are collected out of
// markup, why the matching is deliberately permissive — is `tests/cssOrphans.ts`,
// shared since #130 with the same guard over the Code Intelligence split. This
// file is the settings window's inputs and its allowlist.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { scanSplit } from './cssOrphans';
import { rel, REPO_ROOT } from './repoFiles';

const SECTIONS_DIR = join(REPO_ROOT, 'src', 'lib', 'settings', 'sections');

/**
 * Classes that legitimately have no rule, each with the reason.
 *
 * Every row here is a class the markup carries for a reason OTHER than
 * styling, or a leftover that predates the split — verified against
 * `SettingsApp.svelte` as it stood BEFORE the extraction (`4d697cd`), where
 * none of them had a rule either. A row is a claim that the class is not
 * supposed to be styled; `the allowlist has no stale rows` below fails if one
 * ever acquires a rule, so the list cannot quietly outlive its reasons.
 *
 * Adding a row is the wrong first move: the usual right answer for a newly
 * flagged class is that its rule stayed behind in the parent.
 */
const NO_RULE_EXPECTED = new Map<string, string>([
  [
    'secondary',
    'semantic marker on de-emphasised buttons; has never had a rule in the settings window (the *View components style their own .secondary, scoped)',
  ],
  [
    'reset-arrangement',
    'dead since before the split — the arrangement reset button styles as a plain section button',
  ],
  [
    'linkish',
    'dead since before the split — the Code Audit doc link renders with the default anchor styling',
  ],
  ['about-section', 'decorative wrapper on the About page; layout comes from the section chrome'],
  [
    'promote-overrides',
    'decorative wrapper around the theme-override promote row; layout comes from the section chrome',
  ],
  ['field', 'unstyled label wrapper in Sandboxing; the chrome sheet lays the row out'],
  [
    'policy-env',
    'unstyled wrapper in Offload; only its child .policy-env-label carries a rule',
  ],
]);

describe('settings section CSS', () => {
  const { findings, children, classes, shared } = scanSplit({
    childrenDir: SECTIONS_DIR,
    parentFile: join(REPO_ROOT, 'src', 'SettingsApp.svelte'),
    sharedSheets: [join(REPO_ROOT, 'src', 'lib', 'settings', 'settings-chrome.css')],
  });

  test('the scan actually sees the sections (vacuity guard)', () => {
    // 21 sections and ~250 class uses at the time of writing. The floors are
    // deliberately far below that: their job is to make "walk found nothing"
    // and "the sheets did not load" fail here rather than read as a clean bill
    // of health, not to police the section count.
    expect(children, `${rel(SECTIONS_DIR)} yielded no components`).toBeGreaterThan(15);
    expect(classes, 'no classes were collected — the markup parse is broken').toBeGreaterThan(150);
    // The shared sheets must actually have been read: a typo in a path would
    // otherwise turn every styled class into a finding, and the allowlist
    // would grow to hide it.
    expect(shared.has('hint'), 'settings-chrome.css did not reach the scan').toBe(true);
    expect(shared.has('icon'), 'no injected theme sheet reached the scan').toBe(true);
  });

  test('no section uses a class whose rule stayed behind in SettingsApp (#129)', () => {
    // The extraction defect itself, and it is never benign: the allowlist does
    // NOT apply here. A hit means the markup moved into a child and the rule
    // did not follow — move the rule into that section's `<style>` block, or
    // into `settings-chrome.css` if more than one section needs it.
    const orphans = findings
      .filter((f) => f.inParent)
      .map((f) => `${f.child}  .${f.cls}  (rule is in src/SettingsApp.svelte's scoped block)`);
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
      'These classes have no rule in the section itself, in settings-chrome.css / theme.css / ' +
        'app.css, in an injected theme sheet, or in any :global(...) — so they style nothing. ' +
        'Add the rule where the markup is, or (if the class is deliberately unstyled) add a row ' +
        'to NO_RULE_EXPECTED with the reason:\n' +
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

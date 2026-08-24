// Guard: every class a Settings SECTION puts in its markup must have a rule
// somewhere that can actually reach it.
//
// Why this exists (#129, the SettingsApp split). Twenty-one sections were cut
// out of one 9,000-line component, markup and CSS together. Svelte scopes a
// component's `<style>` to that component's own markup, so a rule left behind
// in `SettingsApp.svelte` while its markup moved into a section child stops
// applying — silently. Nothing catches that:
//
//   - `svelte-check` reports an UNUSED selector (a rule with no markup) and is
//     therefore loud about the leftover half in the parent only if the parent
//     no longer uses it at all — it never reports the other direction, a class
//     in the markup with no rule to match it;
//   - the build is happy either way, and the page renders — just unstyled, in
//     one section, on one screen a maintainer may not open for months.
//
// The extraction is done, so the value now is regression: the next section that
// moves, or the next rule someone relocates into `settings-chrome.css`, gets
// checked at test time instead of at "why is this row suddenly flat".
//
// # Where a rule may live
//
// A class used in `src/lib/settings/sections/*.svelte` resolves if a rule
// naming it exists in any of:
//
//   1. that same section's own `<style>` block — scoped to it, so this is the
//      only per-component source that counts;
//   2. `src/lib/settings/settings-chrome.css` — the settings window's shared
//      form/chrome sheet, imported unscoped by `SettingsApp.svelte`;
//   3. `src/theme.css` and `src/app.css` — imported by `settings_main.ts`;
//   4. the theme sheets injected at runtime, which are unscoped and DO reach
//      section markup: the backend-compiled `tui` theme and every external
//      `themes/<id>/theme.css`. (`.icon` lives here, in the
//      `button:not(.icon)::before` rules that suppress the TUI bracket
//      decoration — a real rule keyed off the class, not an allowlist case.)
//   5. a `:global(...)` selector in ANY component's `<style>` — scoping is
//      lifted by construction there.
//
// Deliberately NOT a source: another section's, or `SettingsApp.svelte`'s,
// scoped `<style>` block. A rule sitting there is precisely the defect.
//
// Rules are matched permissively (`.name` not followed by a word character or
// `-`, anywhere in the file's text). A false "has a rule" only weakens the
// guard; a false "no rule" would block a correct change, and this test must
// never be the reason a correct change is red.
//
// CRLF: read line-ending agnostically — CI checks the repo out with CRLF on
// Windows while local trees are mixed (the V35 lesson).
//
// Why it lives in `tests/`: it reads the repo off disk, and `tsconfig.json`
// type-checks `src/**` without `@types/node`. Same reasoning, and the same
// `node:fs` + walk shape, as `tests/cssTokens.test.ts`.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { read, rel, REPO_ROOT, walk } from './repoFiles';

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

/** Everything from `\n<style>` on — a Svelte component's scoped rules. */
function styleBlock(src: string): string {
  const at = src.indexOf('\n<style>');
  return at >= 0 ? src.slice(at) : '';
}

/** Everything before the `<style>` block — script and markup. */
function markupOf(src: string): string {
  const at = src.indexOf('\n<style>');
  const head = at >= 0 ? src.slice(0, at) : src;
  return head.replace(/<!--[\s\S]*?-->/g, ''); // a class named in a comment is not a use
}

/**
 * The classes a component's markup puts on the page.
 *
 * Three forms, and the third is where care is needed:
 *
 *   class="a b"            → both;
 *   class:active={…}       → `active`;
 *   class="row {x ? 'on' : ''}"  and  class={cond ? 'a' : 'b'}
 *                          → the bare tokens outside the braces, plus the
 *                            contents of every STRING LITERAL inside them.
 *
 * Bare identifiers inside an expression (`class={cls}`) are deliberately NOT
 * collected: `cls` is a variable, not a class, and treating it as one would
 * make this test red over correct code — the failure mode the header rules
 * out. Two more cases go the same way, both live in `HarnessSection`:
 *
 *   - a literal that is one side of a COMPARISON (`kind === 'silent' ? …`) is
 *     a value being tested, not a class being applied;
 *   - a token glued to an interpolation (`class="tier tier-{cap.tier}"`)
 *     names a family of classes, not one — `tier-` alone is not a selector.
 *     The static half (`tier`) is still collected.
 *
 * Each is under-collection in exchange for never being wrong.
 */
function classesUsed(markup: string): Set<string> {
  const found = new Set<string>();
  const add = (text: string): void => {
    for (const token of text.split(/\s+/)) {
      if (/^[A-Za-z][A-Za-z0-9_-]*$/.test(token)) found.add(token);
    }
  };

  for (const m of markup.matchAll(/\bclass:([A-Za-z][\w-]*)/g)) found.add(m[1]);

  for (const m of markup.matchAll(/\bclass=/g)) {
    const value = attributeValue(markup, m.index + m[0].length);
    if (value === null) continue;
    if (value.braced) {
      addExpressionLiterals(value.text, add);
    } else {
      // A quoted attribute may still interpolate. Blank each `{…}` out with a
      // marker so a token touching one is recognisable, and read the
      // expression's own literals separately.
      for (const inner of value.text.matchAll(/\{([^}]*)\}/g)) addExpressionLiterals(inner[1], add);
      // MARKER is not a token character, so `tier tier-{cap.tier}` becomes
      // `tier tier-<marker>`: the glued half is rejected by `add`'s identifier
      // test, while the free-standing token beside it survives.
      add(value.text.replace(/\{[^}]*\}/g, MARKER));
    }
  }
  return found;
}

/**
 * Stands in for an interpolation so a token glued to one stops looking like a
 * class name. Any character outside `[A-Za-z0-9_-]` would do.
 */
const MARKER = '\u0000';

/** The string literals in a `class={…}` expression that are plausibly classes. */
function addExpressionLiterals(expr: string, add: (text: string) => void): void {
  const applied = expr
    // `kind === 'silent'` / `'silent' !== kind` — a compared value.
    .replace(/[!=]==?\s*(["'`])[^"'`\n]*\1/g, ' ')
    .replace(/(["'`])[^"'`\n]*\1\s*[!=]==?/g, ' ')
    // `list.includes('x')`, `id.startsWith('x')` — likewise.
    .replace(/\.\s*(includes|startsWith|endsWith|indexOf|has)\(\s*(["'`])[^"'`\n]*\2/g, ' ');
  for (const lit of applied.matchAll(/["'`]([^"'`\n]*)["'`]/g)) add(lit[1]);
}

/**
 * The attribute value starting at `i` — either `"…"`/`'…'` or a `{…}`
 * expression, brace-counted so a nested object or a ternary with a block
 * doesn't truncate it. Returns null for a shorthand (`{class}`) or anything
 * else unparseable; a missed attribute only under-collects.
 */
function attributeValue(src: string, i: number): { text: string; braced: boolean } | null {
  const open = src[i];
  if (open === '"' || open === "'") {
    const end = src.indexOf(open, i + 1);
    return end < 0 ? null : { text: src.slice(i + 1, end), braced: false };
  }
  if (open !== '{') return null;
  let depth = 0;
  for (let j = i; j < src.length; j += 1) {
    if (src[j] === '{') depth += 1;
    else if (src[j] === '}') {
      depth -= 1;
      if (depth === 0) return { text: src.slice(i + 1, j), braced: true };
    }
  }
  return null;
}

/**
 * Every class name a stylesheet's text NAMES, as a set.
 *
 * Same permissive rule as the per-class `RegExp` this replaces (V42 tranche-2
 * review, T2-10) — `.name` where the name runs to the first character outside
 * the class charset, so `.foo` does not match a rule for `.foo-bar` — but built
 * once per source and queried by lookup, rather than recompiled for every one
 * of the ~250 classes the sections use. The charset is deliberately the one
 * `classesUsed` collects, or the two halves could disagree about where a name
 * ends.
 */
function classTokens(css: string): Set<string> {
  const out = new Set<string>();
  for (const m of css.matchAll(/\.([A-Za-z][\w-]*)/g)) out.add(m[1]);
  return out;
}

/**
 * The unscoped sheets that reach section markup, concatenated. Missing files
 * contribute nothing; the vacuity guard below is what notices if that is all
 * of them.
 */
function unscopedCss(): string {
  const files = [
    join(REPO_ROOT, 'src', 'lib', 'settings', 'settings-chrome.css'),
    join(REPO_ROOT, 'src', 'theme.css'),
    join(REPO_ROOT, 'src', 'app.css'),
    ...walk(join(REPO_ROOT, 'src-tauri', 'src', 'theming'), ['.css']),
    ...walk(join(REPO_ROOT, 'themes'), ['.css']),
  ];
  const parts: string[] = [];
  for (const f of files) {
    try {
      parts.push(read(f));
    } catch {
      /* optional */
    }
  }
  // …plus every `:global(…)` selector in the frontend, whose scoping is lifted.
  for (const f of walk(join(REPO_ROOT, 'src'), ['.svelte'])) {
    for (const g of read(f).matchAll(/:global\(([^)]*)\)/g)) parts.push(g[1]);
  }
  return parts.join('\n');
}

type Finding = { section: string; cls: string; inParent: boolean };

/**
 * One pass over the sections: every class with no rule that can reach it.
 *
 * The shared-sheet token set is returned rather than recomputed by the vacuity
 * guard below — reading every theme sheet and every `.svelte` file in the repo
 * twice to answer the same question was the second half of T2-10.
 */
function scan(): {
  findings: Finding[];
  sections: number;
  classes: number;
  shared: Set<string>;
} {
  const shared = classTokens(unscopedCss());
  const parentScoped = classTokens(
    styleBlock(read(join(REPO_ROOT, 'src', 'SettingsApp.svelte'))),
  );
  const findings: Finding[] = [];
  let classes = 0;
  const files = walk(SECTIONS_DIR, ['.svelte']);
  for (const file of files) {
    const src = read(file);
    const own = classTokens(styleBlock(src));
    const used = classesUsed(markupOf(src));
    classes += used.size;
    for (const cls of used) {
      if (own.has(cls) || shared.has(cls)) continue;
      findings.push({ section: rel(file), cls, inParent: parentScoped.has(cls) });
    }
  }
  return { findings, sections: files.length, classes, shared };
}

describe('settings section CSS', () => {
  const { findings, sections, classes, shared } = scan();

  test('the scan actually sees the sections (vacuity guard)', () => {
    // 21 sections and ~250 class uses at the time of writing. The floors are
    // deliberately far below that: their job is to make "walk found nothing"
    // and "the sheets did not load" fail here rather than read as a clean bill
    // of health, not to police the section count.
    expect(sections, `${rel(SECTIONS_DIR)} yielded no components`).toBeGreaterThan(15);
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
      .map((f) => `${f.section}  .${f.cls}  (rule is in src/SettingsApp.svelte's scoped block)`);
    expect(
      orphans,
      'These classes are used in a section child while their only rule sits in the PARENT\'s ' +
        'scoped <style>, so the rule matches nothing and the markup renders unstyled:\n' +
        orphans.join('\n'),
    ).toEqual([]);
  });

  test('every class a section uses has a rule that reaches it', () => {
    const unresolved = findings
      .filter((f) => !NO_RULE_EXPECTED.has(f.cls))
      .map((f) => `${f.section}  .${f.cls}`);
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

// The machinery behind the "a class in a child's markup must have a rule that
// can reach it" guards.
//
// Why it is shared (#130). `settingsCssOrphans.test.ts` was written for ONE
// split (#129, SettingsApp → 21 section children). #130 performed the same
// operation on `CodeIntelligenceView.svelte` — hoist the cross-section rules to
// an unscoped sheet keyed on the parent's root class, then move sections out
// markup-and-CSS-together — and inherited the same silent failure mode, which
// nothing else catches:
//
//   - `svelte-check` reports an UNUSED selector (a rule with no markup). It is
//     loud about the half left behind in the parent only when the parent stops
//     using it entirely, and it never reports the other direction — a class in
//     the markup with no rule to match it;
//   - the build is happy either way, and the page renders. Just unstyled, in
//     one section, on a screen a maintainer may not open for months.
//
// One definition, two callers, same reasoning as `repoFiles.ts` (V42
// tranche-2, T2-10): a scan that grows a case in one copy and not the other is
// a guard that quietly stops looking at part of the tree.
//
// # Where a rule may live
//
// A class used in a child component resolves if a rule naming it exists in:
//
//   1. that same child's own `<style>` block — scoped to it, so this is the
//      only per-component source that counts;
//   2. one of the UNSCOPED sheets the caller names (the split's shared sheet:
//      `settings-chrome.css`, `codeIntel.css`) plus `src/theme.css` and
//      `src/app.css`;
//   3. the theme sheets injected at runtime, which are unscoped and DO reach
//      child markup: the backend-compiled `tui` theme and every external
//      `themes/<id>/theme.css`. (`.icon` lives here, in the
//      `button:not(.icon)::before` rules that suppress the TUI bracket
//      decoration — a real rule keyed off the class, not an allowlist case.)
//   4. a `:global(...)` selector in ANY component's `<style>` — scoping is
//      lifted by construction there.
//
// Deliberately NOT a source: another child's, or the PARENT's, scoped `<style>`
// block. A rule sitting in the parent while its markup moved is precisely the
// defect these guards exist to catch, and `Finding.inParent` marks it.
//
// Rules are matched permissively (`.name` where the name runs to the first
// character outside the class charset, so `.foo` does not match a rule for
// `.foo-bar`). A false "has a rule" only weakens the guard; a false "no rule"
// would block a correct change, and these guards must never be the reason a
// correct change is red.

import { join } from 'node:path';

import { read, rel, REPO_ROOT, walk } from './repoFiles';

/** Everything from `\n<style>` on — a Svelte component's scoped rules. */
export function styleBlock(src: string): string {
  const at = src.indexOf('\n<style>');
  return at >= 0 ? src.slice(at) : '';
}

/** Everything before the `<style>` block — script and markup. */
export function markupOf(src: string): string {
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
 * make these guards red over correct code — the failure mode the header rules
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
export function classesUsed(markup: string): Set<string> {
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
 * Built once per source and queried by lookup, rather than a `RegExp`
 * recompiled for every class the children use (V42 tranche-2 review, T2-10).
 * The charset is deliberately the one `classesUsed` collects, or the two halves
 * could disagree about where a name ends.
 */
export function classTokens(css: string): Set<string> {
  const out = new Set<string>();
  for (const m of css.matchAll(/\.([A-Za-z][\w-]*)/g)) out.add(m[1]);
  return out;
}

/**
 * The unscoped sheets that reach child markup, concatenated: the caller's own
 * shared sheet(s) first, then the app-wide ones and the runtime-injected
 * themes. Missing files contribute nothing; each caller's vacuity guard is what
 * notices if that is all of them.
 */
function unscopedCss(sharedSheets: string[]): string {
  const files = [
    ...sharedSheets,
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

/** One class, in one child, with no rule that can reach it. */
export type Finding = {
  /** Repo-relative path of the child component. */
  child: string;
  cls: string;
  /** The rule EXISTS, in the parent's scoped block — the extraction defect. */
  inParent: boolean;
};

export type Scan = {
  findings: Finding[];
  /** How many child components the walk found (for the vacuity guard). */
  children: number;
  /** How many class USES were collected across them (likewise). */
  classes: number;
  /** Class names the unscoped sheets name — the vacuity guard probes this. */
  shared: Set<string>;
};

/**
 * Scan one split: every class used by a child under `childrenDir` that no rule
 * reachable from that child names.
 *
 * `childFilter` lets a caller scan a directory that holds more than the split's
 * children. `parentFile` is the component the markup came OUT of, and is used
 * only to label a finding as the extraction defect rather than a missing rule.
 */
export function scanSplit(opts: {
  childrenDir: string;
  parentFile: string;
  sharedSheets: string[];
  childFilter?: (file: string) => boolean;
}): Scan {
  const shared = classTokens(unscopedCss(opts.sharedSheets));
  const parentScoped = classTokens(styleBlock(read(opts.parentFile)));
  const findings: Finding[] = [];
  let classes = 0;
  const files = walk(opts.childrenDir, ['.svelte']).filter(opts.childFilter ?? (() => true));
  for (const file of files) {
    const src = read(file);
    const own = classTokens(styleBlock(src));
    const used = classesUsed(markupOf(src));
    classes += used.size;
    for (const cls of used) {
      if (own.has(cls) || shared.has(cls)) continue;
      findings.push({ child: rel(file), cls, inParent: parentScoped.has(cls) });
    }
  }
  return { findings, children: files.length, classes, shared };
}

import { describe, expect, it } from 'vitest';
import { inAnchoredPopover } from './anchoredPopover';

/// A stand-in for the one DOM call the predicate makes. The suite runs without
/// a DOM, and the value under test is the SELECTOR — that it asks for a panel
/// (`.menu` with a menu/dialog role) and not for "any form control", which is
/// the distinction #147's fix rests on: the tab strip's tabs are `<button>`s,
/// and suppressing the terminal refocus for those would break clicking a tab in
/// an unfocused pane.
function node(matches: string[]): { closest(selector: string): unknown } {
  return {
    closest: (selector: string) =>
      matches.some((m) => selector.includes(m)) ? {} : null,
  };
}

describe('inAnchoredPopover', () => {
  it('matches a panel by class AND role, the shape all three popovers render', () => {
    expect(inAnchoredPopover(node(['.menu[role="dialog"]']))).toBe(true);
    expect(inAnchoredPopover(node(['.menu[role="menu"]']))).toBe(true);
  });

  it('is false for a node in no panel, and for nothing at all', () => {
    expect(inAnchoredPopover(node([]))).toBe(false);
    expect(inAnchoredPopover(null)).toBe(false);
    expect(inAnchoredPopover(undefined)).toBe(false);
  });

  it('does not ask about form controls — a tab is a <button> inside no panel', () => {
    expect(inAnchoredPopover(node(['button', 'input']))).toBe(false);
  });
});

// `quarantineReason` — the substantiveness predicate behind the Memory view's
// quarantine card (#48, F-24).
//
// The card used to show a held note's text, time and Promote/Discard and nothing
// about the cause. Three different causes land in that queue (the session taint
// latch, an unattributable write, and the write-time credential screen), and for
// the third one the note TEXT is the credential — so the card displayed the
// secret value and withheld the rule name that matched it, which inverts locked
// decision 22.
//
// This predicate exists so the card has exactly ONE way to ask "is there a
// reason to show?", and so "empty" cannot slip through as "explained". It is
// tested here because `.svelte` files have no test harness in this repo.

import { describe, it, expect } from 'vitest';
import { quarantineReason, type MemNote } from './graph';

function note(over: Partial<MemNote> = {}): MemNote {
  return {
    note_id: 'n1',
    session_id: 's1',
    text: 'AKIAIOSFODNN7EXAMPLE',
    ts_ms: 1_000,
    pinned: false,
    tainted: true,
    ...over,
  };
}

describe('quarantineReason', () => {
  it('returns the reason when the backend sent one', () => {
    const q = {
      screen: 'memory_quarantine',
      rules: ['secret_aws_access_key_id'],
      reason: 'The write-time credential screen matched this note.',
    };
    expect(quarantineReason(note({ quarantine: q }))).toBe(q);
  });

  it('keeps a rule-less reason — two of the three causes match no rule', () => {
    // The latch and unattributed-write holds have no rule identifier at all.
    // Requiring one here would suppress a real, actionable reason.
    const q = {
      screen: 'memory_quarantine',
      rules: [],
      reason: 'Written while this session had already used an external tool.',
    };
    expect(quarantineReason(note({ quarantine: q }))).toBe(q);
  });

  it('treats a MISSING field as "no reason to show"', () => {
    // Every build whose backend predates the field. The card must say "not
    // recorded", not invent a cause.
    expect(quarantineReason(note())).toBeNull();
    expect(quarantineReason(note({ quarantine: null }))).toBeNull();
  });

  it('treats a PRESENT-BUT-EMPTY reason as absent, not as an explanation', () => {
    // "Empty is not absent" — collapsed toward the honest end on purpose: a
    // blank reason means "we cannot tell you why", so it must render as the
    // not-recorded line rather than as an empty explanation that reads like the
    // note had no cause. A blank arriving here is a backend defect, and this is
    // what keeps it visible.
    for (const reason of ['', '   ', '\n\t ']) {
      expect(
        quarantineReason(note({ quarantine: { screen: 'memory_quarantine', rules: [], reason } })),
      ).toBeNull();
    }
  });

  it('does not invent a reason from the note text', () => {
    // The note body here IS an AWS key. Nothing on this side may re-screen it
    // and name a rule — a plausible-but-wrong cause in a security UI is worse
    // than a blank field (#48, F-23 is that exact mistake elsewhere in the app).
    expect(quarantineReason(note({ text: 'AKIAIOSFODNN7EXAMPLE' }))).toBeNull();
  });
});

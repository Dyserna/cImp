import { describe, expect, test } from 'vitest';

import { navigateBack, navigateForward, type HistoryState } from './history';

describe('preview back-stack history', () => {
  test('A -> B -> C, Back, Back reaches A (no oscillation)', () => {
    let state: HistoryState = { backStack: [], current: 'A' };

    state = navigateForward(state, 'B');
    expect(state).toEqual({ backStack: ['A'], current: 'B' });

    state = navigateForward(state, 'C');
    expect(state).toEqual({ backStack: ['A', 'B'], current: 'C' });

    // V14 code-review FIX 9 regression: a Back navigation must not re-push
    // the url being left, or Back would only ever oscillate between the
    // last two urls (C <-> B) instead of walking further back to A.
    state = navigateBack(state);
    expect(state).toEqual({ backStack: ['A'], current: 'B' });

    state = navigateBack(state);
    expect(state).toEqual({ backStack: [], current: 'A' });
  });

  test('navigateBack on an empty stack is a no-op', () => {
    const state: HistoryState = { backStack: [], current: 'A' };
    expect(navigateBack(state)).toEqual(state);
  });

  test('a forward navigation after going back can build fresh history', () => {
    let state: HistoryState = { backStack: [], current: 'A' };
    state = navigateForward(state, 'B');
    state = navigateForward(state, 'C');
    state = navigateBack(state); // back to B, backStack = [A]
    // Navigating forward to a new page D from B pushes B, not C.
    state = navigateForward(state, 'D');
    expect(state).toEqual({ backStack: ['A', 'B'], current: 'D' });
  });
});

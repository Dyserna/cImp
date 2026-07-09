// V14 code-review fix (FIX 9): pure back-stack model for the Preview
// toolbar's Back button, extracted out of `PreviewToolbar.svelte` so the
// push-vs-no-push rule is unit-testable without mounting the component.
//
// WebView2 exposes no wry-visible navigation-history API this toolbar can
// reach, so the toolbar keeps its own simple stack of previously-left URLs.
//
// The bug this fixes: the component's `navigateTo()` used to push the
// CURRENT url onto the back-stack unconditionally, including when the
// navigation was itself triggered by the Back button — so Back only ever
// bounced between the last two URLs (A -> B -> C, Back, Back landed back on
// C instead of reaching A) and could never walk further back through
// history. `navigateForward` (used for typed/submitted/linked navigation)
// pushes; `navigateBack` (used for the Back button) does not.

export interface HistoryState {
  /** URLs previously left, most-recent-last. */
  backStack: string[];
  /** The URL currently displayed. */
  current: string;
}

/** Forward navigation (URL bar submit, a link click forwarded from the
 * webview, etc.): push the url being LEFT (`state.current`) onto the stack,
 * then move `current` to `to`. */
export function navigateForward(state: HistoryState, to: string): HistoryState {
  return { backStack: [...state.backStack, state.current], current: to };
}

/** Back-button navigation: pop the most recent stack entry and move
 * `current` there WITHOUT re-pushing the url being left — this is what
 * fixes the oscillation bug. A no-op (returns `state` unchanged) when the
 * stack is empty. */
export function navigateBack(state: HistoryState): HistoryState {
  if (state.backStack.length === 0) return state;
  const backStack = state.backStack.slice(0, -1);
  const current = state.backStack[state.backStack.length - 1];
  return { backStack, current };
}

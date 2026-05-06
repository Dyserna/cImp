// Layout sizing constants. Used by the splitter resize handler in
// Split.svelte to clamp drag positions, and by the same component's
// render-time clamp that keeps stored ratios visually clamped when the
// window shrinks below 2 * min_size without overwriting the user's
// preference. Values are deliberately small — the goal is "don't let a
// pane disappear," not "enforce a comfortable working size."

/// Minimum pane width when a horizontal split is active. Below this
/// threshold the resize drag clamps; below twice this in the parent
/// container, the render-clamp falls back to a 50/50 split.
export const MIN_PANE_WIDTH_PX = 200;

/// Minimum pane height for vertical splits. Smaller than the width
/// minimum because terminal output is line-oriented — a few rows is
/// still useful, where a few columns is not.
export const MIN_PANE_HEIGHT_PX = 100;

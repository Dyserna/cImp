// Cross-component handle for "focus the xterm.js terminal." Terminal.svelte
// installs the focus function on mount and clears it on destroy; the compose
// overlay (and any future caller) invokes it without needing a direct
// reference to the xterm instance.

let focusFn: (() => void) | null = null;

export function setTerminalFocuser(fn: (() => void) | null): void {
  focusFn = fn;
}

export function focusTerminal(): void {
  focusFn?.();
}

// Per-tab terminal registry.
//
// Owns one xterm.js Terminal + FitAddon + PTY wiring per tab id. Each
// terminal's host element lives offscreen by default and is portaled
// into a pane's content slot on activation. Moving the host element
// between slots preserves xterm state, scrollback, and the bytes
// channel — recreating any of those would lose the running session.
//
// Lifecycle:
//   create(tabId)   — invoked from the same hook points as
//                      `applyTabCreated` (snapshot in App.svelte,
//                      tab-created event in avatarState.ts). Builds the
//                      host, instantiates xterm, opens the PTY, wires
//                      every per-tab listener.
//   destroy(tabId)  — invoked from the tab-closed handler. Runs every
//                      teardown step the v1.2 Terminal.svelte's
//                      onDestroy did, in the same order.
//   attach(tabId, slot) — appendChild the host into a slot. Fits xterm
//                      to the slot's pixel size and focuses on the next
//                      animation frame.
//   detach(tabId)   — moves the host back to the offscreen container.
//
// xterm.js survives parent changes because the Terminal object holds
// a reference to its `term.open(host)` element regardless of where
// that element is mounted in the DOM tree. The fit/focus pair on
// every attach catches up on size changes that happened while the
// host was offscreen (the ResizeObserver intentionally skips
// zero-sized hosts to avoid mangling scrollback).

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { CanvasAddon } from '@xterm/addon-canvas';
import '@xterm/xterm/css/xterm.css';
import './terminals.css';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { get } from 'svelte/store';

import {
  createBytesChannel,
  decodeBase64,
  onPtyExit,
  ptyResize,
  ptyRestart,
  ptyStart,
  ptyWrite,
  restartShellTab,
  type BytesChannel,
} from './ipc';
import {
  display as displaySettings,
  settings as settingsStore,
} from './settings/store';
import { effectiveTheme, themeFromSetting } from './themes/resolve';
import {
  applyHostBackgroundCss,
  categoryOf,
  composeTheme,
  effectiveBackgroundMode,
} from './terminal/background';
import { setTerminalFocuser } from './terminalFocus';
import { perTabClosedState } from './avatarState';
import { clearTabError, setTabError } from './tabs/errorState';
import { isShellTab, type TabId } from './tabs/types';

const OFFSCREEN_ID = 'terminal-offscreen';

let offscreenEl: HTMLDivElement | null = null;

/// Lazily create the offscreen container. The container is a single
/// DOM-wide stash for hosts that are not currently attached to a pane;
/// keeping it absolute-positioned far off the visible window is what
/// the M1 spec calls a "portal" pattern.
function ensureOffscreen(): HTMLDivElement {
  if (offscreenEl && offscreenEl.isConnected) return offscreenEl;
  const existing = document.getElementById(OFFSCREEN_ID) as HTMLDivElement | null;
  if (existing) {
    offscreenEl = existing;
    return existing;
  }
  const el = document.createElement('div');
  el.id = OFFSCREEN_ID;
  el.style.position = 'absolute';
  el.style.left = '-10000px';
  el.style.top = '-10000px';
  el.style.width = '1px';
  el.style.height = '1px';
  el.style.overflow = 'hidden';
  el.style.visibility = 'hidden';
  document.body.appendChild(el);
  offscreenEl = el;
  return el;
}

interface TerminalEntry {
  tabId: TabId;
  host: HTMLDivElement;
  term: Terminal;
  fitAddon: FitAddon;
  /// Live `tab-pty-exit` payload listener. Unsubscribed on destroy.
  unlistenExit: UnlistenFn | null;
  /// Live `tab-restart-requested` payload listener. Unsubscribed on
  /// destroy.
  unlistenRestart: UnlistenFn | null;
  /// Subscribers to the display + closed-state stores. Run at create
  /// time; all unsub at destroy.
  unsubFont: () => void;
  unsubClosed: (() => void) | null;
  /// Subscribes to the full settings store and keeps both theme and
  /// background in sync. Theme is always live (V1.4-01: `term.options
  /// .theme = next`); background is live for color/opacity/blur/etc.
  /// changes within the same renderer category, but a category flip
  /// (fast ↔ image) triggers a debounced full Terminal recreate
  /// (V1.4-02). See the V1.4-02 plan for the matrix.
  unsubAppearance: () => void;
  /// Renderer category currently active on this Terminal — `'fast'`
  /// for the canvas-with-no-image path (mode 'none' or 'color') and
  /// `'image'` for the DOM-with-image path. The settings subscriber
  /// compares the next mode's category against this to decide between
  /// in-place update and full recreate.
  bgCategory: 'fast' | 'image';
  /// Mirrors the closed flag for this tab so the onData handler — which
  /// closes over its initial value — sees the latest state without
  /// rebinding xterm.
  isClosed: boolean;
  /// Guards the restart listener against the dual-trigger race: closed
  /// overlay Enter and the context-menu Restart entry can both fire in
  /// quick succession.
  restarting: boolean;
  /// Resize debounce timer keyed in this entry so we can cancel on
  /// destroy.
  resizeTimer: ReturnType<typeof setTimeout> | null;
  resizeObserver: ResizeObserver | null;
  /// True when the host is attached to a real pane slot, false when
  /// offscreen. Fits are gated on this — the offscreen container has a
  /// non-zero pixel box (offset* > 0) so a pure visibility-based check
  /// would mistakenly fit hosts to a few columns wide while they wait
  /// for first activation.
  attached: boolean;
  /// Bytes channel currently bound to xterm.write. Each restart rebinds
  /// to a fresh Channel because the Tauri bridge can only target one
  /// receiver per channel.
  bytesChannel: BytesChannel;
}

const entries = new Map<TabId, TerminalEntry>();

function displayNameFor(t: TabId): string {
  if (t === 'claude') return 'Claude Code';
  if (t === 'aider') return 'Aider';
  return 'Shell';
}

/// True when the entry's host has real layout — attached to a slot
/// AND has non-zero offset dimensions. xterm.js's FitAddon mangles its
/// grid when the host is undersized (returns ~2 cols if the parent is
/// the 1px offscreen stash, broadcasts a tiny SIGWINCH to the child),
/// so every fit guards on the explicit attached flag rather than just
/// the offset check.
function hostIsFittable(entry: TerminalEntry): boolean {
  if (!entry.attached) return false;
  const { host } = entry;
  return host.offsetWidth > 0 && host.offsetHeight > 0;
}

function fitAndResize(entry: TerminalEntry): void {
  if (!hostIsFittable(entry)) return;
  entry.fitAddon.fit();
  const { rows, cols } = entry.term;
  ptyResize(entry.tabId, rows, cols).catch((e) =>
    console.error('pty_resize failed:', e),
  );
}

function bindBytesChannel(entry: TerminalEntry): BytesChannel {
  const channel = createBytesChannel();
  channel.onmessage = (encoded) => {
    entry.term.write(decodeBase64(encoded));
  };
  entry.bytesChannel = channel;
  return channel;
}

async function attemptSpawn(entry: TerminalEntry, restart: boolean): Promise<void> {
  const channel = bindBytesChannel(entry);
  const { rows, cols } = entry.term;
  try {
    if (restart) {
      await ptyRestart(entry.tabId, channel, rows, cols);
    } else {
      await ptyStart(entry.tabId, channel, rows, cols);
    }
    clearTabError(entry.tabId);
    entry.term.focus();
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    setTabError(entry.tabId, {
      headline: `${displayNameFor(entry.tabId)} failed to start.`,
      raw: msg,
    });
    console.error(`pty_start failed for ${entry.tabId}:`, e);
  }
}

/// Create the per-tab terminal. Idempotent: a second call for the same
/// tab is a no-op (the snapshot path in App.svelte and the runtime
/// `tab-created` event can both hit this for the same id during
/// startup).
///
/// `options.restartPty` (V1.4-02): when true, the spawn path uses
/// `pty_restart` instead of `pty_start`. This is the recreate-on-
/// background-toggle flow — destroyTerminal kills the JS side
/// (xterm + listeners) but the backend PTY survives, so a fresh
/// `pty_start` would either error or duplicate the process.
/// `pty_restart` shuts down the existing PTY and spawns a new one.
export function createTerminal(
  tabId: TabId,
  options: { restartPty?: boolean } = {},
): void {
  if (entries.has(tabId)) return;

  const offscreen = ensureOffscreen();

  // Resolve the tab's effective theme + background mode. The tab may
  // not be in `settings.tabs` yet during the snapshot/event race at
  // startup; in that case we fall back to global settings alone.
  const initialSettings = get(settingsStore);
  const initialTab = initialSettings.tabs.find((t) => t.id === tabId);
  const baseTheme = initialTab
    ? effectiveTheme(initialTab, initialSettings.terminal.theme)
    : themeFromSetting(initialSettings.terminal.theme);
  const initialMode = effectiveBackgroundMode(
    initialTab ?? { background_override: null },
    initialSettings.terminal.background,
  );
  const initialTheme = composeTheme(baseTheme, initialMode);
  const initialCategory = categoryOf(initialMode);

  const host = document.createElement('div');
  host.className = 'terminal-host';
  host.dataset.tabId = tabId;
  // Brief paint between term.open(host) and the first xterm frame: use
  // the resolved theme bg so a Dracula tab doesn't flash black.
  host.style.background = initialTheme.background ?? '#000';
  applyHostBackgroundCss(host, initialMode);
  offscreen.appendChild(host);

  const display = get(displaySettings);
  const term = new Terminal({
    fontFamily: display.terminal_font_family,
    fontSize: display.terminal_font_size,
    cursorBlink: true,
    allowProposedApi: true,
    theme: initialTheme,
    // V1.4-02: image mode requires transparency so the CSS image
    // beneath the cells layer is visible. Color-only and 'none' modes
    // skip this so the canvas renderer paints opaque cells (faster).
    ...(initialCategory === 'image' ? { allowTransparency: true } : {}),
  });
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);
  // V1.4-02: canvas renderer for the fast path (no image). Image mode
  // stays on the in-core DOM renderer — the canvas addon is a single
  // opaque surface and would obscure the CSS image beneath.
  if (initialCategory !== 'image') {
    term.loadAddon(new CanvasAddon());
  }
  term.open(host);

  // Placeholder bytes channel — replaced by `bindBytesChannel` inside
  // `attemptSpawn`. The placeholder lets us satisfy the entry's type
  // without a nullable field that every helper would have to guard.
  const placeholderChannel = createBytesChannel();

  const entry: TerminalEntry = {
    tabId,
    host,
    term,
    fitAddon,
    unlistenExit: null,
    unlistenRestart: null,
    unsubFont: () => {},
    unsubClosed: null,
    unsubAppearance: () => {},
    bgCategory: initialCategory,
    isClosed: false,
    restarting: false,
    resizeTimer: null,
    resizeObserver: null,
    attached: false,
    bytesChannel: placeholderChannel,
  };
  entries.set(tabId, entry);

  // Live font subscription — applies font changes in place. Skips the
  // initial dispatch because xterm was constructed with the current
  // values above.
  let firstFont = true;
  entry.unsubFont = displaySettings.subscribe((d) => {
    if (firstFont) {
      firstFont = false;
      return;
    }
    if (
      term.options.fontFamily !== d.terminal_font_family ||
      term.options.fontSize !== d.terminal_font_size
    ) {
      term.options.fontFamily = d.terminal_font_family;
      term.options.fontSize = d.terminal_font_size;
      fitAndResize(entry);
    }
  });

  // V1.4-02: live appearance subscription — recomputes both theme and
  // background mode on every settings change.
  //
  //   - When the renderer category stays the same (fast↔fast or
  //     image↔image), updates apply in place: theme reassignment +
  //     CSS variable updates on the host. xterm.js diffs colors
  //     internally so identical themes are a no-op.
  //   - When the renderer category flips (fast↔image), the Terminal
  //     must be recreated — the canvas addon is loaded once at
  //     construction and `allowTransparency` is constructor-only.
  //     `queueRecreate` debounces so live slider drags during a global
  //     edit don't thrash, and the recreate path uses pty_restart so
  //     the still-running PTY is rebound to the new xterm.
  //
  // Skips the initial dispatch (xterm was constructed with the resolved
  // theme + mode above).
  let firstAppearance = true;
  entry.unsubAppearance = settingsStore.subscribe((s) => {
    if (firstAppearance) {
      firstAppearance = false;
      return;
    }
    const tab = s.tabs.find((t) => t.id === tabId);
    const baseTheme = tab
      ? effectiveTheme(tab, s.terminal.theme)
      : themeFromSetting(s.terminal.theme);
    const mode = effectiveBackgroundMode(
      tab ?? { background_override: null },
      s.terminal.background,
    );

    if (categoryOf(mode) !== entry.bgCategory) {
      queueRecreate(tabId);
      return;
    }

    const nextTheme = composeTheme(baseTheme, mode);
    term.options.theme = nextTheme;
    if (nextTheme.background) {
      host.style.background = nextTheme.background;
    }
    applyHostBackgroundCss(host, mode);
  });

  term.onData((data) => {
    if (entry.isClosed) {
      // Shell-tab closed-state intercept: Enter triggers a restart, all
      // other input is dropped on the floor (the PTY is gone, writing
      // would error).
      if (data === '\r' || data === '\n' || data === '\r\n') {
        void restartShellTab(tabId).catch((e) =>
          console.error('restart_shell_tab failed:', e),
        );
      }
      return;
    }
    ptyWrite(tabId, data).catch((e) => console.error('pty_write failed:', e));
  });

  if (isShellTab(tabId)) {
    entry.unsubClosed = perTabClosedState.subscribe((m) => {
      entry.isClosed = m[tabId]?.closed ?? false;
    });
  }

  setTerminalFocuser(tabId, () => term.focus());

  void onPtyExit((payload) => {
    if (payload.tab !== tabId) return;
    term.write(`\r\n[${tabId} exited: ${payload.exit}]\r\n`);
    if (isShellTab(tabId)) return;
    setTabError(tabId, {
      headline: `${displayNameFor(tabId)} exited unexpectedly.`,
      raw: payload.exit,
    });
  })
    .then((unlisten) => {
      // Tab may have been destroyed before the listener returned —
      // detach immediately if so.
      if (entries.get(tabId) !== entry) {
        unlisten();
        return;
      }
      entry.unlistenExit = unlisten;
    })
    .catch((e) => console.error('listen pty-exit failed:', e));

  void listen<TabId>('tab-restart-requested', async (event) => {
    if (event.payload !== tabId) return;
    if (entries.get(tabId) !== entry) return;
    if (entry.restarting) return;
    entry.restarting = true;
    try {
      term.write(`\r\n[restarting ${tabId}…]\r\n`);
      await attemptSpawn(entry, true);
    } finally {
      entry.restarting = false;
    }
  })
    .then((unlisten) => {
      if (entries.get(tabId) !== entry) {
        unlisten();
        return;
      }
      entry.unlistenRestart = unlisten;
    })
    .catch((e) => console.error('listen tab-restart-requested failed:', e));

  // Track host size; refit when visible. The visibility guard prevents
  // a tiny SIGWINCH when the host is detached (offscreen has 1px×1px).
  entry.resizeObserver = new ResizeObserver(() => {
    if (entry.resizeTimer) clearTimeout(entry.resizeTimer);
    entry.resizeTimer = setTimeout(() => {
      entry.resizeTimer = null;
      fitAndResize(entry);
    }, 250);
  });
  entry.resizeObserver.observe(host);

  void attemptSpawn(entry, options.restartPty ?? false);
}

/// V1.4-02 recreate-on-toggle debounce. A category flip
/// (fast ↔ image) requires a full Terminal recreation because the
/// canvas addon and `allowTransparency` are construction-time
/// decisions. Live slider drags during a global edit can fire many
/// settings updates per second; debouncing collapses them into a
/// single recreate after the user pauses.
const recreateTimers = new Map<TabId, ReturnType<typeof setTimeout>>();

function queueRecreate(tabId: TabId): void {
  const existing = recreateTimers.get(tabId);
  if (existing) clearTimeout(existing);
  recreateTimers.set(
    tabId,
    setTimeout(() => {
      recreateTimers.delete(tabId);
      // The previous PTY is still alive — pty_restart shuts it down
      // and respawns. Scrollback resets, per the V1.4-02 contract.
      destroyTerminal(tabId);
      createTerminal(tabId, { restartPty: true });
    }, 120),
  );
}

/// Tear down a tab's terminal. Destroys xterm, unsubscribes every
/// listener, removes the host. Idempotent.
export function destroyTerminal(tabId: TabId): void {
  const entry = entries.get(tabId);
  if (!entry) return;
  entries.delete(tabId);

  // Cancel any pending background-toggle recreate so a tab close
  // mid-debounce doesn't resurrect the terminal a few hundred ms
  // later.
  const pendingRecreate = recreateTimers.get(tabId);
  if (pendingRecreate) {
    clearTimeout(pendingRecreate);
    recreateTimers.delete(tabId);
  }

  if (entry.resizeTimer) clearTimeout(entry.resizeTimer);
  entry.resizeObserver?.disconnect();
  entry.unlistenExit?.();
  entry.unlistenRestart?.();
  entry.unsubFont();
  entry.unsubClosed?.();
  entry.unsubAppearance();
  setTerminalFocuser(tabId, null);
  // xterm.dispose() can throw if internal state was already partially
  // torn down by a rapid create-then-destroy or by a host element
  // removed out from under it. Registry entry is already deleted above,
  // so a swallowed error here can't leave dangling references — but an
  // uncaught throw bubbles to the console as an unhandled error. Trace
  // log instead.
  try {
    entry.term.dispose();
  } catch (e) {
    console.warn(`xterm dispose for ${tabId} threw:`, e);
  }
  if (entry.host.parentElement) {
    entry.host.parentElement.removeChild(entry.host);
  }
}

/// Move the tab's host into `slot` and re-fit/focus on the next
/// animation frame. The rAF lets layout stabilize before
/// `fitAddon.fit()` measures pixel dimensions — without it, a freshly
/// portaled host can fit at the wrong size when the slot's flexbox
/// hasn't yet propagated.
export function attachTerminal(tabId: TabId, slot: HTMLElement): void {
  const entry = entries.get(tabId);
  if (!entry) return;
  if (entry.host.parentElement !== slot) {
    slot.appendChild(entry.host);
  }
  entry.attached = true;
  requestAnimationFrame(() => {
    if (entries.get(tabId) !== entry) return;
    if (!entry.attached) return;
    fitAndResize(entry);
    entry.term.focus();
  });
}

/// Move the tab's host back to the offscreen container. When `fromSlot`
/// is provided, the move is conditional on the host actually being in
/// that slot — this prevents an unmounting pane from "stealing back" a
/// host that another pane has already taken ownership of during a tree
/// rearrangement (e.g. split-pane, where the original pane unmounts
/// after the new pane has already attached the dragged tab).
export function detachTerminal(tabId: TabId, fromSlot?: HTMLElement): void {
  const entry = entries.get(tabId);
  if (!entry) return;
  if (fromSlot && entry.host.parentElement !== fromSlot) return;
  entry.attached = false;
  const offscreen = ensureOffscreen();
  if (entry.host.parentElement !== offscreen) {
    offscreen.appendChild(entry.host);
  }
}

/// Trigger a manual respawn — invoked by `TabErrorOverlay`'s Retry
/// button. The retry flag passes through to `pty_restart`, which
/// idempotently shuts down any stale handle before respawning.
export async function retryTerminal(tabId: TabId): Promise<void> {
  const entry = entries.get(tabId);
  if (!entry) return;
  await attemptSpawn(entry, true);
}

/// Whether the registry is tracking a terminal for `tabId`. Pane
/// components use this to gate slot mounting until the registry has
/// caught up with a freshly-arrived `tab-created` event.
export function hasTerminal(tabId: TabId): boolean {
  return entries.has(tabId);
}

/// Focus the tab's xterm. Called from the Pane component when the pane
/// becomes focused without its active tab changing — without this, a
/// focus shift between panes whose active tabs are already attached
/// wouldn't move keyboard focus to the new pane's terminal.
export function focusTerminalFor(tabId: TabId): void {
  const entry = entries.get(tabId);
  if (!entry) return;
  entry.term.focus();
}

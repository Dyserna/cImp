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
//   attach(tabId, slot) — appendChild the host into a slot. Acquires the
//                      WebGL context (M17), fits xterm to the slot's pixel
//                      size, and focuses on the next animation frame.
//   detach(tabId)   — releases the WebGL context (M17), then moves the host
//                      back to the offscreen container.
//
// M17 renderer policy: a WebGL2 context is a scarce per-process resource
// (WebView2 caps them at ~16 and evicts the LRU past that), so it is bound
// to VISIBILITY, not to a terminal's lifetime — attach loads the addon,
// detach disposes it, everything stashed offscreen paints via xterm's
// in-core DOM renderer. See `shouldHoldWebgl` in terminal/background.ts.
//
// xterm.js survives parent changes because the Terminal object holds
// a reference to its `term.open(host)` element regardless of where
// that element is mounted in the DOM tree. The fit/focus pair on
// every attach catches up on size changes that happened while the
// host was offscreen (the ResizeObserver intentionally skips
// zero-sized hosts to avoid mangling scrollback).

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { SerializeAddon } from '@xterm/addon-serialize';
import '@xterm/xterm/css/xterm.css';
import './terminals.css';
import { listenEvent, TAB_RESTART_REQUESTED } from './events';
import {
  readText as clipboardReadText,
  writeText as clipboardWriteText,
} from '@tauri-apps/plugin-clipboard-manager';
import { get } from 'svelte/store';

import {
  createBytesChannel,
  decodeBase64,
  onPtyExit,
  ptyRebindChannel,
  ptyResize,
  ptyRestart,
  ptyStart,
  ptyWrite,
  restartShellTab,
  type BytesChannel,
} from './ipc';
import { beginSelectionTts } from './selectionTts';
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
  recreateDebounceDelay,
  shouldHoldWebgl,
} from './terminal/background';
import { setTerminalFocuser } from './terminalFocus';
import { perTabClosedState } from './avatarState';
import { openConfigureTabDialog } from './dialog/store';
import { clearTabError, setTabError } from './tabs/errorState';
import { isShellTab, isAppRenderedTab, type TabId } from './tabs/types';
import { isReservedAiTab, tabLabel } from './harness';
import {
  courtesyRefusal,
  readOnlyAdvice,
  readOnlyExempt,
  readOnlyRefusalMessage,
} from './delegation';
import { isPromptRelaxed } from './delegationPrompt';
import { showToast } from './toast';

/// V39 Phase A: when each tab last told the user its keyboard is locked.
///
/// A refused keystroke is a per-KEY event and the notice is a per-INTENT one:
/// without this, holding a key down would stack a hundred toasts for one
/// misunderstanding. First refusal speaks immediately; repeats inside the
/// window are silent (the input is still refused — only the notice is
/// throttled).
const READ_ONLY_TOAST_GAP_MS = 4000;
const lastReadOnlyToastAt = new Map<TabId, number>();

function noteReadOnlyRefusal(tabId: TabId, message: string): void {
  const now = Date.now();
  const last = lastReadOnlyToastAt.get(tabId) ?? 0;
  if (now - last < READ_ONLY_TOAST_GAP_MS) return;
  lastReadOnlyToastAt.set(tabId, now);
  // V39 Phase B: the ADVICE follows the reason. A tab refused because a
  // delegation is driving it is not unlocked by the access radio — pointing
  // its owner there would name a disabled control (`readOnlyAdvice`).
  showToast(`${message} ${readOnlyAdvice(message)}`);
}

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
  /// V1.4-03: captures xterm scrollback as a single ANSI escape stream
  /// before a renderer-category flip destroys this Terminal. The new
  /// xterm replays the snapshot via `term.write` so the user sees their
  /// shell history continue across the flip.
  serializeAddon: SerializeAddon;
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
  /// xterm.js `onSelectionChange` listener disposable. Drives the
  /// behavior.copy_on_select feature — see the registration block in
  /// `createTerminal`.
  selectionListener: { dispose: () => void } | null;
  /// Renderer category currently active on this Terminal — `'fast'`
  /// for the WebGL-with-no-image path (mode 'none' or 'color') and
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
  /// V29/M17: the live WebGL renderer addon, or `null` when this Terminal
  /// is on xterm's in-core DOM renderer. Non-null only while the terminal
  /// is visible AND eligible — see `syncWebglRenderer` / `shouldHoldWebgl`.
  /// (Pre-M17 this handle lived in a closure because it was never unloaded;
  /// the visibility policy makes load/dispose a per-attach operation, so the
  /// entry has to own it.)
  webglAddon: WebglAddon | null;
  /// M17: sticky "WebGL2 is not usable on this Terminal" flag. Set when
  /// `loadAddon` throws (no WebGL2 context: GPU blocklist, RDP, headless)
  /// or when the one permitted context-loss retry also lost its context.
  /// Deliberately survives stash→show cycles: without it, every tab switch
  /// would re-probe a machine that already proved it has no usable WebGL,
  /// re-emitting the DOM-fallback warning forever. Cleared only by a full
  /// Terminal recreate (renderer flip) or a tab restart.
  webglFailed: boolean;
  /// One-shot context-loss retry latch for the current *visible* session.
  /// Reset on detach, so a terminal that recovers after being stashed gets
  /// a fresh retry budget the next time it comes on screen; never reset by
  /// the retry itself (that would loop against a resetting driver).
  webglRetried: boolean;
}

const entries = new Map<TabId, TerminalEntry>();

/// V0.6+ module-scope listeners. Pre-V0.6 each `createTerminal` call
/// registered its own `pty-exit` and `tab-restart-requested` listener
/// that filtered by tab id inside the callback — with N tabs every
/// backend emit ran N JS callbacks, and a quick create-then-destroy
/// could drop a not-yet-resolved listener (`unlistenExit` would still
/// be null when destroy ran). One listener per event, dispatched via
/// the `entries` map by tab id, fixes both: O(1) dispatch and a single
/// install/teardown that doesn't depend on listener resolution order.
let moduleListenersInstalled = false;
async function ensureModuleListeners(): Promise<void> {
  if (moduleListenersInstalled) return;
  moduleListenersInstalled = true;
  try {
    await onPtyExit((payload) => {
      const entry = entries.get(payload.tab as TabId);
      if (!entry) return;
      entry.term.write(`\r\n[${entry.tabId} exited: ${payload.exit}]\r\n`);
      if (isShellTab(entry.tabId)) return;
      setTabError(entry.tabId, {
        headline: `${displayNameFor(entry.tabId)} exited unexpectedly.`,
        raw: payload.exit,
      });
    });
  } catch (e) {
    console.error('listen pty-exit failed:', e);
  }
  try {
    await listenEvent(TAB_RESTART_REQUESTED, async (event) => {
      const entry = entries.get(event.payload);
      if (!entry || entry.restarting) return;
      // V1.4-03: a Restart click during a debounced recreate would
      // otherwise produce two PTY operations — drop any pending recreate
      // for this tab; the restart is the user's intent.
      const pendingRecreate = recreateTimers.get(entry.tabId);
      if (pendingRecreate) {
        clearTimeout(pendingRecreate);
        recreateTimers.delete(entry.tabId);
      }
      entry.restarting = true;
      try {
        entry.term.write(`\r\n[restarting ${entry.tabId}…]\r\n`);
        await attemptSpawn(entry, 'restart');
      } finally {
        entry.restarting = false;
      }
    });
  } catch (e) {
    console.error('listen tab-restart-requested failed:', e);
  }
}

/// The name the in-tab error card calls this tab — the harness's label for a
/// reserved built-in AI tab, "Shell" for everything else.
///
/// V40 Phase F (locked decision 7): a lookup in the registry rather than a
/// branch per shipped tab id, so a harness added after this build gets its own
/// name in "X failed to start." instead of being called a shell.
///
/// V40 review L-13: a RESERVED AI tab id whose label is not known yet is called
/// by its id, not "Shell". `tabLabel` answers `''` until `harness_list` lands,
/// and a spawn failure is the tightest race there is — it happens at startup —
/// so "**Shell** failed to start." was landing in a reserved AI tab's error
/// card.
function displayNameFor(t: TabId): string {
  return isReservedAiTab(t) ? tabLabel(t) || t : 'Shell';
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

/**
 * V20: true when the running program has enabled mouse tracking (DECSET
 * 1000/1002/1003/1006), i.e. it owns the pointer — the case for a fullscreen
 * AI TUI. xterm exposes this via `term.modes.mouseTrackingMode` ('none' when
 * off). Used to decide whether cImp's right-click paste would double-act with
 * the app's own mouse handling. Defensive: any access error reads as "off".
 */
function isMouseTrackingActive(term: Terminal): boolean {
  try {
    const mode = (term as unknown as { modes?: { mouseTrackingMode?: string } })
      .modes?.mouseTrackingMode;
    return !!mode && mode !== 'none';
  } catch {
    return false;
  }
}

/** DECSET/DECRST private modes that enable mouse reporting (X10/VT200/drag/
 * any-event tracking + the SGR/urxvt coordinate encodings). */
const MOUSE_TRACKING_MODES = new Set([1000, 1001, 1002, 1003, 1005, 1006, 1015, 1016]);

/**
 * V20: keep the mouse LOCAL in a fullscreen AI tab so cImp's selection works
 * like it does in a shell, with a **hold-Alt bypass** to hand the mouse to the
 * app when the user actually wants it.
 *
 * Both shipped harnesses enable mouse tracking (DECSET 1000/1002/1003/1006)
 * in their fullscreen TUI, which routes drags/clicks to the app — breaking
 * copy-on-select, right-click paste, and select-to-speak (no local selection
 * ever forms, so `getSelection()` is empty → "no text selected"). We intercept
 * the mouse-tracking mode set/reset sequences:
 *
 *  - **Default (Alt up):** swallow them, so xterm never forwards mouse events
 *    and all gestures are local (select/copy/paste/speak), exactly like a shell.
 *  - **While Alt is held:** re-enable exactly the modes the app asked for, so
 *    drags/clicks reach the app (on Windows, Alt is not an xterm selection
 *    modifier, so the events forward cleanly); released → back to local.
 *
 * Other private modes (alt-screen 1049, bracketed paste 2004, cursor 25, focus
 * 1004) always pass through. AI tabs only; shell tabs are never altered. The
 * key listeners live on `host` (which xterm's textarea sits inside), so they're
 * garbage-collected with the host on teardown — no explicit cleanup needed.
 */
function installAiMouseControl(
  term: Terminal,
  host: HTMLElement,
  tabId: string,
): void {
  const appModes = new Set<number>(); // mouse modes the app currently wants on
  let passthrough = false; // true while Alt is held (forward to app)
  let injecting = false; // true while WE write an enable/disable to xterm

  const allMouse = (params: (number | number[])[]): number[] | null => {
    if (params.length === 0) return null;
    const nums = params.map((p) => (Array.isArray(p) ? p[0] : p));
    return nums.every((n) => MOUSE_TRACKING_MODES.has(n)) ? nums : null;
  };

  const makeHandler =
    (final: 'h' | 'l') =>
    (params: (number | number[])[]): boolean => {
      if (injecting) return false; // our own enable/disable — let xterm apply it
      const nums = allMouse(params);
      if (!nums) return false; // not a mouse-only sequence — leave it alone
      for (const n of nums) {
        if (final === 'h') appModes.add(n);
        else appModes.delete(n);
      }
      // Swallow by default (stay local). While passing through, apply the
      // app's live changes so its mode stays current.
      return !passthrough;
    };

  try {
    term.parser.registerCsiHandler({ prefix: '?', final: 'h' }, makeHandler('h'));
    term.parser.registerCsiHandler({ prefix: '?', final: 'l' }, makeHandler('l'));
  } catch (e) {
    console.warn('mouse-tracking control registration failed:', e);
    return;
  }

  const setXtermMouse = (on: boolean): void => {
    if (appModes.size === 0) return; // app never asked for the mouse
    const list = [...appModes].join(';');
    injecting = true;
    term.write(`\x1b[?${list}${on ? 'h' : 'l'}`, () => {
      injecting = false;
    });
  };

  host.addEventListener('keydown', (e) => {
    if (e.key === 'Alt' && !passthrough && appModes.size > 0) {
      passthrough = true;
      setXtermMouse(true);
    }
  });
  host.addEventListener('keyup', (e) => {
    if (e.key === 'Alt' && passthrough) {
      passthrough = false;
      setXtermMouse(false);
    }
  });
  // Focus leaving the terminal (Alt+Tab, click elsewhere) can swallow the Alt
  // keyup — reset so the mouse doesn't stay stuck in passthrough.
  host.addEventListener('focusout', () => {
    if (passthrough) {
      passthrough = false;
      setXtermMouse(false);
    }
  });

  // V20: forward the mouse WHEEL to the app even though we suppress the app's
  // mouse-tracking modes (to keep click/drag selection local). Without this,
  // xterm — alt buffer, mouse tracking off — translates the wheel into
  // cursor-arrow keys (ESC O A / ESC O B), which the harness TUIs treat as input
  // navigation, not scrolling. We synthesize the exact wheel sequence the app's
  // chosen encoding expects (SGR 1006 or legacy X10) and write it straight to
  // the PTY, so scroll works natively while clicks/drags stay local (copy-on-
  // select and right-click paste keep working).
  term.attachCustomWheelEventHandler((e) => {
    // App owns the mouse (Alt-held passthrough): let xterm encode the wheel.
    if (passthrough) return true;
    // App never asked for the mouse (e.g. a normal-buffer program in an AI
    // tab): let xterm scroll its own scrollback as usual.
    if (appModes.size === 0) return true;
    if (e.deltaY === 0) return true;
    const { col, row } = wheelCell(term, e);
    const seq = wheelSequence(e.deltaY < 0, appModes, col, row);
    // One notch per DOM wheel event for a mouse; for high-resolution (pixel)
    // deltas — trackpads — send a few proportional notches, capped so a fast
    // flick can't flood the PTY.
    const notches =
      e.deltaMode === WheelEvent.DOM_DELTA_PIXEL
        ? Math.min(5, Math.max(1, Math.round(Math.abs(e.deltaY) / 40)))
        : 1;
    ptyWrite(tabId, seq.repeat(notches)).catch((err) =>
      console.error('wheel-forward pty_write failed:', err),
    );
    // xterm does NOT call preventDefault when a custom wheel handler returns
    // false — it just skips its own handling — so consume the event ourselves,
    // otherwise the browser default (overscroll / WebView2 gesture-nav) fires.
    e.preventDefault();
    return false; // stop xterm's arrow-key (alternate-scroll) translation
  });
}

/**
 * V20: the terminal cell under a wheel event, 1-based. TUIs that hit-test the
 * wheel by coordinate (one shipped TUI routes it to the pane under the pointer;
 * a fixed (1;1) lands on chrome that doesn't scroll) need the real cell.
 * Falls back to (1;1) when the screen element isn't measurable.
 */
function wheelCell(term: Terminal, e: WheelEvent): { col: number; row: number } {
  const screen = term.element?.querySelector('.xterm-screen');
  const rect = screen?.getBoundingClientRect();
  if (!rect || rect.width <= 0 || rect.height <= 0) return { col: 1, row: 1 };
  const clamp = (v: number, max: number): number => Math.min(max, Math.max(1, v));
  return {
    col: clamp(Math.ceil(((e.clientX - rect.left) / rect.width) * term.cols), term.cols),
    row: clamp(Math.ceil(((e.clientY - rect.top) / rect.height) * term.rows), term.rows),
  };
}

/**
 * V20: encode a single mouse-wheel notch the way a TUI expects it. Wheel-up is
 * button 64, wheel-down 65, reported at the given cell — one shipped TUI scrolls
 * the pane under the pointer, so the coordinate matters (the other scrolls the
 * transcript regardless). Uses the SGR (1006) encoding when the app enabled
 * it, else the legacy X10 form (each byte offset by 32, coords capped at its
 * 223 maximum).
 */
function wheelSequence(
  up: boolean,
  appModes: Set<number>,
  col: number,
  row: number,
): string {
  const button = up ? 64 : 65;
  if (appModes.has(1006)) {
    return `\x1b[<${button};${col};${row}M`;
  }
  return `\x1b[M${String.fromCharCode(32 + button)}${String.fromCharCode(
    32 + Math.min(col, 223),
  )}${String.fromCharCode(32 + Math.min(row, 223))}`;
}

/**
 * V29/M17: bring this terminal's renderer in line with the WebGL policy —
 * **only visible terminals hold a WebGL2 context** (rationale + the
 * context-cap arithmetic live on `shouldHoldWebgl` in
 * `terminal/background.ts`; decision D-7b in
 * `docs/MILESTONE-V29-xterm6-renderer.md` records the policy).
 *
 * Idempotent, so every attach/detach can just call it. The two transitions
 * are driven from `attachTerminal` / `detachTerminal` — the single seam
 * where a host moves between a pane slot and the offscreen stash — and from
 * the context-loss handler.
 */
function syncWebglRenderer(entry: TerminalEntry): void {
  const want = shouldHoldWebgl(entry);
  if (want === (entry.webglAddon !== null)) return;
  if (want) loadWebglRenderer(entry);
  else unloadWebglRenderer(entry);
}

/**
 * V29: loads the WebGL renderer addon onto an already-opened Terminal.
 *
 * Must run AFTER `term.open(host)` — but not because open() could fail:
 * xterm 6's `open()` wraps the `onWillOpen` fire in a swallowing try/catch,
 * so a pre-open addon whose deferred `activate` throws is silently eaten and
 * the terminal quietly ends up on the DOM renderer. That silence is exactly
 * the problem. Post-open `loadAddon` activates *synchronously*, so the throw
 * ("WebGL2 not supported" — GPU blocklist, RDP, headless) surfaces to the
 * try/catch below, which is what lets us (a) latch `webglFailed` so we stop
 * re-probing and (b) emit the DOM-fallback `console.warn` — the only signal
 * that a machine is running without GPU acceleration.
 *
 * Context-loss policy: dispose the lost addon (xterm reverts to the DOM
 * renderer on its own; buffer, PTY, and listeners survive) and attempt
 * exactly ONE fresh load per visible session. If that retry loses its
 * context too, this Terminal latches `webglFailed` and stays on DOM — no
 * retry loop against a resetting driver, and no re-probe on every tab
 * switch. `webglFailed` is per-`TerminalEntry`, so anything that builds a new
 * Terminal — a renderer-flip recreate (`queueRecreate`) or closing and
 * reopening the tab — re-attempts. A PTY restart does not: it reuses the same
 * Terminal, and re-probing a driver that just refused us buys nothing.
 */
function loadWebglRenderer(entry: TerminalEntry): void {
  const { term, tabId } = entry;
  const addon = new WebglAddon();
  // Registered before loadAddon: activation is what wires the renderer's
  // context-loss event through, and a load that throws must still leave a
  // consistent (disposed) addon behind.
  addon.onContextLoss(() => {
    // The loss fires 3 s after the browser drops the context, so by now the
    // tab may have been closed, recreated (V1.4-03 renderer flip), or
    // stashed offscreen (which disposed this addon already).
    try {
      addon.dispose();
    } catch {
      /* already torn down with the Terminal — nothing to do */
    }
    const live = entries.get(tabId);
    if (!live || live.term !== term) return;
    if (live.webglAddon !== addon) return;
    live.webglAddon = null;
    if (live.webglRetried) {
      live.webglFailed = true;
      console.warn(
        `WebGL context lost twice for tab ${tabId}; ` +
          'terminal falls back to the DOM renderer.',
      );
      return;
    }
    live.webglRetried = true;
    // Re-checks the policy: a tab stashed during the 3 s loss timer stays on
    // DOM until it is shown again.
    syncWebglRenderer(live);
  });
  try {
    term.loadAddon(addon);
    entry.webglAddon = addon;
  } catch (e) {
    // Leaves the addon registered-but-inert in xterm's AddonManager
    // otherwise: loadAddon pushes before it calls activate().
    addon.dispose();
    entry.webglAddon = null;
    entry.webglFailed = true;
    console.warn(
      `WebGL renderer unavailable for tab ${tabId}; ` +
        'terminal falls back to the DOM renderer:',
      e,
    );
  }
}

/**
 * M17: release this terminal's WebGL2 context. `WebglAddon.dispose()` hands
 * the RenderService a fresh in-core DOM renderer and removes the WebGL
 * canvas, so the terminal keeps painting — buffer, PTY, listeners, and
 * scrollback are untouched (this is the same path the context-loss fallback
 * already used). Never fires `onContextLoss`: that event comes from the
 * canvas's `webglcontextlost` listener, which disposal removes.
 */
function unloadWebglRenderer(entry: TerminalEntry): void {
  const addon = entry.webglAddon;
  if (!addon) return;
  entry.webglAddon = null;
  try {
    addon.dispose();
  } catch (e) {
    console.warn(`WebGL addon dispose for ${entry.tabId} threw:`, e);
  }
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

type SpawnMode = 'start' | 'restart' | 'rebind';

async function attemptSpawn(entry: TerminalEntry, mode: SpawnMode): Promise<void> {
  const channel = bindBytesChannel(entry);
  const { rows, cols } = entry.term;
  try {
    if (mode === 'rebind') {
      // V1.4-03: re-point the still-running PTY at the new xterm. The
      // shell session, env, cwd, and processes survive. If the PTY has
      // exited (or never started), pty_rebind_channel errors out and we
      // fall back to a fresh pty_start so the tab is still usable —
      // user sees scrollback reset and a brief shell respawn.
      try {
        await ptyRebindChannel(entry.tabId, channel);
      } catch (rebindErr) {
        console.warn(
          `pty_rebind fell back to pty_start for ${entry.tabId}:`,
          rebindErr,
        );
        // V1.4-04 D.5: pty_start returns persisted scrollback bytes.
        // Discard them on the rebind-fallback path — the new xterm
        // already has the V1.4-03 serialize-replay drawn into it,
        // and writing the persisted bytes would land them after the
        // fresher session content (out of chronological order).
        // The bytes are gone after this call (consumed server-side);
        // that's acceptable for the rare fallback case.
        //
        // Re-read the geometry here rather than reusing the pre-await `rows`/
        // `cols`: on a renderer-flip recreate the new Terminal is built at the
        // old geometry and only fit on attach, so the snapshot above can be the
        // pre-fit size. Spawning at the live size avoids a full-screen TUI
        // briefly drawing at the wrong column count before SIGWINCH catches up.
        await ptyStart(entry.tabId, channel, entry.term.rows, entry.term.cols);
      }
    } else if (mode === 'restart') {
      await ptyRestart(entry.tabId, channel, rows, cols);
    } else {
      // V1.4-04 D.5: cold-start path. If the previous session
      // persisted scrollback for this tab on graceful exit, the
      // backend returns it here; we replay it into the new xterm
      // *before* the next microtask cycle so the bytes land before
      // any live PTY output that arrives via the bound channel.
      const restored = await ptyStart(entry.tabId, channel, rows, cols);
      if (restored && restored.length > 0) {
        const text = new TextDecoder('utf-8', { fatal: false }).decode(
          new Uint8Array(restored),
        );
        entry.term.write(text);
      }
    }
    clearTabError(entry.tabId);
    entry.term.focus();
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    setTabError(entry.tabId, {
      headline: `${displayNameFor(entry.tabId)} failed to start.`,
      raw: msg,
    });
    console.error(`pty spawn (${mode}) failed for ${entry.tabId}:`, e);
  }
}

/// Create the per-tab terminal. Idempotent: a second call for the same
/// tab is a no-op (the snapshot path in App.svelte and the runtime
/// `tab-created` event can both hit this for the same id during
/// startup).
///
/// `options.restartPty` (V1.4-02, kept for the user-initiated Restart
/// path): when true, the spawn path uses `pty_restart` instead of
/// `pty_start`. Shuts down the existing PTY and spawns a new one.
///
/// `options.rebindPty` (V1.4-03): renderer-category flip path. The
/// existing PTY survives — `pty_rebind_channel` re-points its bytes at
/// the new xterm without restarting the shell. `scrollbackSnapshot`
/// and `initialGeometry` carry the visible state from the previous
/// xterm so the user sees their session continue across the flip.
export function createTerminal(
  tabId: TabId,
  options: {
    restartPty?: boolean;
    rebindPty?: boolean;
    scrollbackSnapshot?: string;
    initialGeometry?: { rows: number; cols: number };
  } = {},
): void {
  if (entries.has(tabId)) return;
  // App-rendered tabs (reserved dashboards, Note, Preview) render Svelte
  // content or an embedded webview instead of an xterm — no terminal entry.
  // One shared predicate so a new app-rendered tab can't miss this guard.
  if (isAppRenderedTab(tabId)) return;

  const offscreen = ensureOffscreen();

  // Resolve the tab's effective theme + background mode. The tab may
  // not be in `settings.tabs` yet during the snapshot/event race at
  // startup; in that case we fall back to global settings alone.
  const initialSettings = get(settingsStore);
  const initialTabRaw = initialSettings.tabs.find((t) => t.id === tabId);
  // V14 Phase F: `PreviewTabConfig` has neither `theme_override` nor
  // `background_override` (no terminal to theme) — narrowed out here so
  // `effectiveTheme`/`effectiveBackgroundMode`'s structural param types are
  // satisfied. Unreachable in practice (the guard above already returns
  // before a Preview tab ever gets here), but the type-level exclusion
  // documents why AiTool/Shell are the only kinds these resolvers see.
  const initialTab =
    initialTabRaw && initialTabRaw.kind !== 'preview' ? initialTabRaw : undefined;
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
  // ── V32 Phase D — OSC 52 clipboard-write audit (2026-08-06) ──────────────
  // VERDICT: a clipboard hijack via an escape sequence in displayed output is
  // NOT possible here, and no config change was needed. Evidence, against the
  // pinned @xterm/xterm 6.0.0 in package.json:
  //   * xterm.js core registers OSC handlers for 0, 1, 2, 4, 8, 10, 11, 12,
  //     104, 110, 111, 112 only (verified by reading the shipped
  //     `lib/xterm.js.map` sourcesContent for `src/common/InputHandler.ts`).
  //     There is no OSC 52 handler and no catch-all OSC fallback, so an
  //     `ESC ] 52 ; c ; <base64> BEL` in PTY output is parsed and DISCARDED.
  //   * OSC 52 in xterm.js lives in the optional `@xterm/addon-clipboard`
  //     addon, which this project does not depend on (package.json carries
  //     only addon-fit / addon-serialize / addon-webgl) and never loads.
  //   * cImp registers exactly two parser handlers of its own —
  //     `registerCsiHandler({prefix:'?', final:'h'|'l'})` for DECSET mouse
  //     modes in `installAiMouseControl` — and no OSC handler anywhere.
  //   * `windowOptions` is not set; every sub-option defaults to false, so the
  //     CSI-t window-manipulation reporting channel stays closed too.
  //   * `allowProposedApi: true` below only unlocks proposed APIs for loaded
  //     addons; it does not enable any escape-sequence behaviour by itself.
  // Re-run this audit if a clipboard addon is ever added, if an OSC handler is
  // registered here, or on an xterm major upgrade — those are the three ways
  // the verdict can change. cImp writes to the clipboard only through the
  // Tauri clipboard-manager plugin, from explicit user gestures (copy-on-
  // select, right-click paste), never from terminal output.
  const term = new Terminal({
    fontFamily: display.terminal_font_family,
    fontSize: display.terminal_font_size,
    cursorBlink: true,
    allowProposedApi: true,
    theme: initialTheme,
    // V1.4-02: image mode requires transparency so the CSS image
    // beneath the cells layer is visible. Color-only and 'none' modes
    // skip this so the WebGL renderer paints opaque cells (faster).
    ...(initialCategory === 'image' ? { allowTransparency: true } : {}),
    // V1.4-03: on a renderer recreate, construct at the previous
    // terminal's geometry so the replayed scrollback's cursor positions
    // line up with the new grid. fit-on-attach corrects any minor
    // host-size differences after the snapshot lands.
    ...options.initialGeometry,
  });
  // V20: AI tabs keep the mouse local (shell-like selection: copy-on-select,
  // right-click paste, select-to-speak) by suppressing the fullscreen app's
  // mouse-tracking modes, with a hold-Alt bypass to hand the mouse to the app.
  // Registered before the PTY binds so the app's initial DECSET burst is
  // caught. Shell tabs are untouched.
  if (!isShellTab(tabId)) {
    installAiMouseControl(term, host, tabId);
  }
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);
  // V1.4-03: serialize addon captures scrollback as ANSI for replay
  // across a renderer-flip recreate. Cheap to load even when no replay
  // ever happens.
  const serializeAddon = new SerializeAddon();
  term.loadAddon(serializeAddon);
  term.open(host);
  // V1.4-02 / V29 / M17 renderer policy: the WebGL addon is NOT loaded here.
  // A fresh terminal is born in the offscreen stash (`attached: false`), and
  // only VISIBLE terminals hold a WebGL2 context — `attachTerminal` loads it,
  // `detachTerminal` disposes it. Loading at construction gave every kept-
  // alive tab its own context and blew past WebView2's ~16-context cap; see
  // `shouldHoldWebgl`. Image-background terminals never take the fast path at
  // all (the opaque WebGL canvas would hide the CSS image beneath).

  // V1.4-03: replay the captured scrollback before binding the PTY
  // channel. xterm processes its write queue FIFO, so the snapshot
  // lands before any live byte that arrives once attemptSpawn resolves.
  if (options.scrollbackSnapshot) {
    term.write(options.scrollbackSnapshot);
  }

  // Placeholder bytes channel — replaced by `bindBytesChannel` inside
  // `attemptSpawn`. The placeholder lets us satisfy the entry's type
  // without a nullable field that every helper would have to guard.
  const placeholderChannel = createBytesChannel();

  const entry: TerminalEntry = {
    tabId,
    host,
    term,
    fitAddon,
    serializeAddon,
    unsubFont: () => {},
    unsubClosed: null,
    unsubAppearance: () => {},
    selectionListener: null,
    bgCategory: initialCategory,
    isClosed: false,
    restarting: false,
    resizeTimer: null,
    resizeObserver: null,
    attached: false,
    bytesChannel: placeholderChannel,
    webglAddon: null,
    webglFailed: false,
    webglRetried: false,
  };
  entries.set(tabId, entry);
  // Idempotent: only the first call actually registers. Run it from
  // here rather than at module top-level so the listeners are tied to
  // app-bootstrap timing (after Tauri runtime is ready).
  void ensureModuleListeners();

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
  //     must be recreated — `allowTransparency` is constructor-only, and
  //     `bgCategory` is baked into this Terminal's WebGL eligibility.
  //     `queueRecreate` debounces so live slider drags during a global
  //     edit don't thrash. V1.4-03: the recreate path captures
  //     scrollback via the serialize addon, then uses pty_rebind
  //     (instead of pty_restart) so the shell session, env, cwd, and
  //     processes survive — only the JS xterm is replaced.
  //
  // Skips the initial dispatch (xterm was constructed with the resolved
  // theme + mode above).
  let firstAppearance = true;
  entry.unsubAppearance = settingsStore.subscribe((s) => {
    if (firstAppearance) {
      firstAppearance = false;
      return;
    }
    const tabRaw = s.tabs.find((t) => t.id === tabId);
    // See the matching narrowing comment in `createTerminal` above.
    const tab = tabRaw && tabRaw.kind !== 'preview' ? tabRaw : undefined;
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

  // Copy-on-select: when xterm reports a selection change, push the
  // selected text to the system clipboard. The setting is read at fire
  // time so toggling it in the live settings store takes effect without
  // re-binding. Empty selections (click-to-deselect, mouseup outside the
  // grid) are skipped so we don't blow away whatever the user actually
  // has on the clipboard. xterm fires this event many times during a
  // drag — every fire writes the latest selection, which is fine and
  // matches conventional terminal copy-on-select behavior.
  entry.selectionListener = term.onSelectionChange(() => {
    if (!get(settingsStore).behavior.copy_on_select) return;
    const text = term.getSelection();
    if (!text) return;
    // Tauri/WebView2 gates the web Clipboard API (readText especially); route
    // through the clipboard-manager plugin so copy and paste both go via the
    // native clipboard and behave consistently.
    void clipboardWriteText(text).catch((e) =>
      console.warn('copy-on-select clipboard write failed:', e),
    );
  });

  // Right-click handling. Two distinct gestures share the contextmenu
  // event:
  //   - Ctrl+right-click → speak the current selection aloud (TTS).
  //   - plain right-click → paste the clipboard into the PTY.
  // The Ctrl branch is checked FIRST and always returns, so holding Ctrl
  // can never fall through to the paste branch — the modifier suppresses
  // paste entirely (the user's requirement). Each gesture is
  // independently gated by its own behavior setting; both read settings
  // at fire time so toggles take effect without re-binding.
  host.addEventListener('contextmenu', (e) => {
    const behavior = get(settingsStore).behavior;
    if (e.ctrlKey) {
      // Speak-selection gesture. Even when the feature is off we still
      // swallow the event for Ctrl+right-click so it never pastes — the
      // modifier is reserved for this gesture. The selection is chunked into
      // sentences (so large/multi-line selections aren't truncated) and, when
      // enabled, painted with a receding read-along highlight; see
      // `selectionTts.ts`.
      e.preventDefault();
      if (!behavior.speak_selection_on_right_click) return;
      if (!term.getSelection().trim()) return;
      const highlight = get(settingsStore).tts.selection_highlight;
      void beginSelectionTts(term, highlight).catch((err) =>
        console.warn('speak-selection TTS failed:', err),
      );
      return;
    }
    if (!behavior.paste_on_right_click) return;
    // V20: in a fullscreen AI tab the app enables mouse tracking and owns the
    // pointer, so a plain right-click is already delivered to the app as a
    // mouse event. Pasting on top of that double-acts. When the app owns the
    // mouse, require Shift (xterm's bypass for local gestures) before cImp
    // pastes; suppress the OS menu either way. Shell tabs and normal-buffer
    // programs don't enable mouse tracking, so they paste on a plain
    // right-click exactly as before.
    if (isMouseTrackingActive(term) && !e.shiftKey) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    clipboardReadText()
      .then((text) => {
        if (!text) return;
        term.paste(text);
      })
      .catch((err) =>
        console.warn('paste-on-right-click clipboard read failed:', err),
      );
  });

  term.onData((data) => {
    if (entry.isClosed) {
      // Shell-tab closed-state intercept. Enter routes to Configure
      // when the close was a launch failure (closed_message set —
      // typically command-not-found), because re-running the same
      // broken command will hit the same error. For a normal exit
      // (no closed_message), Enter restarts the subprocess.
      if (data === '\r' || data === '\n' || data === '\r\n') {
        const closed = get(perTabClosedState)[tabId];
        if (closed?.closed_message) {
          openConfigureTabDialog(tabId);
        } else {
          void restartShellTab(tabId).catch((e) =>
            console.error('restart_shell_tab failed:', e),
          );
        }
      }
      return;
    }
    // V39 Phase A: the read-only courtesy gate. The lock is enforced in the
    // backend (`pty_write` refuses with the reason) — this only keeps the
    // round trip out of the common case and gets the notice in front of the
    // user on the FIRST refused keystroke rather than the last.
    //
    // Terminal protocol replies are exempt for the same reason the backend
    // exempts them: they are the terminal answering the running program, and a
    // TUI waiting for a cursor-position report would hang on a swallowed one.
    // Mouse WHEEL reports are exempt too — scrolling is reading, and under an
    // alt-screen TUI the wheel goes to the program rather than to xterm's own
    // buffer. Clicks and drags are not: they activate controls.
    //
    // V39 review R-4: a standing prompt on a DRIVEN tab opens the keyboard for
    // the answer (locked decision 5), whatever the persisted lock says — the
    // backend already allows it, and swallowing here meant the one prompt only
    // the user can answer could not be answered at all.
    const lockReason = courtesyRefusal(
      get(settingsStore),
      tabId,
      isPromptRelaxed(tabId),
    );
    if (lockReason && !readOnlyExempt(data)) {
      noteReadOnlyRefusal(tabId, `This tab is ${lockReason}.`);
      return;
    }
    ptyWrite(tabId, data).catch((e) => {
      // …and surface the backend's own refusal if one arrives anyway — the
      // gate above reads a store that can lag the lock (a tab locked from
      // another window, or the engine's lock in a later phase).
      const refusal = readOnlyRefusalMessage(e);
      if (refusal) {
        noteReadOnlyRefusal(tabId, refusal);
        return;
      }
      console.error('pty_write failed:', e);
    });
  });

  if (isShellTab(tabId)) {
    entry.unsubClosed = perTabClosedState.subscribe((m) => {
      entry.isClosed = m[tabId]?.closed ?? false;
    });
  }

  setTerminalFocuser(tabId, () => term.focus());

  // V0.6+: pty-exit and tab-restart-requested are dispatched from a
  // single module-scope listener (`ensureModuleListeners`). No per-tab
  // listener registration here.

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

  // V1.4-03: route to the right spawn mode. `rebindPty` and
  // `restartPty` are mutually exclusive (the former is the renderer-flip
  // recreate, the latter is the user-initiated Restart action).
  const spawnMode: SpawnMode = options.rebindPty
    ? 'rebind'
    : options.restartPty
      ? 'restart'
      : 'start';
  void attemptSpawn(entry, spawnMode);
}

/// V1.4-02 recreate-on-toggle debounce. A category flip
/// (fast ↔ image) requires a full Terminal recreation because the
/// `allowTransparency` option is a construction-time decision (the M17
/// renderer policy makes the WebGL addon itself load/unload dynamically,
/// but `bgCategory` is still baked in at construction — and the recreate
/// conveniently clears the `webglFailed` latch too).
/// Live slider drags during a global edit can fire many
/// settings updates per second; debouncing collapses them into a
/// single recreate after the user pauses.
const recreateTimers = new Map<TabId, ReturnType<typeof setTimeout>>();

function queueRecreate(tabId: TabId): void {
  const existing = recreateTimers.get(tabId);
  if (existing) clearTimeout(existing);

  // V1.4-04 A.3: per-tab debounce stagger. Formula and rationale live
  // in `recreateDebounceDelay` in `terminal/background.ts`.
  const delay = recreateDebounceDelay(
    Array.from(entries.keys()).indexOf(tabId),
  );

  recreateTimers.set(
    tabId,
    setTimeout(() => {
      recreateTimers.delete(tabId);
      const old = entries.get(tabId);
      if (!old) return;
      // V1.4-03 coordination: a user-initiated Restart that fires
      // during the debounce already cleared this timer, but if a new
      // recreate gets queued while a restart is mid-flight, skip it —
      // the restart already replaced the PTY's child and a recreate
      // would tear down the freshly-restarted xterm.
      if (old.restarting) return;

      // V1.4-03: capture both the scrollback snapshot AND the geometry
      // before destroy. The new Terminal must be constructed at the
      // same rows/cols so the replayed cursor positions land on the
      // right cells; xterm's default 24×80 would wrap a 60×200
      // snapshot badly.
      //
      // V1.4-04 A.1/A.2: bound the snapshot at `snapshot_lines` rows
      // (default 2000) to cap JS-heap allocation under 50k+ scrollback.
      // Skip capture entirely when the alt-screen buffer is active —
      // serializeAddon's replay of alt-screen state can land mid-screen
      // with the alt buffer's content laid over the main buffer's blank
      // canvas, which is worse than a blank canvas the user can fix
      // with Ctrl+L. The PTY rebind preserves the live shell session
      // either way; only the alt-buffer's visible content is dropped.
      const wasAltScreen = old.term.buffer.active.type === 'alternate';
      const cap = get(settingsStore).terminal.background.snapshot_lines;
      const snapshot = wasAltScreen
        ? undefined
        : old.serializeAddon.serialize({ scrollback: cap });
      const { rows, cols } = old.term;

      // V1.4-03: capture the old host's slot before destroy so the
      // freshly-created host can be re-attached to the same pane.
      // Pane.svelte's slot effect short-circuits when
      // `mountedTab === desired` — and that's true here because the
      // tab id is unchanged across the recreate — so it won't attach
      // the new host on its own. Without this re-attach the new host
      // sits in the offscreen container until the user switches tabs
      // away and back.
      const previousParent = old.host.parentElement;
      const previousSlot =
        previousParent &&
        previousParent.classList &&
        previousParent.classList.contains('terminal-slot')
          ? previousParent
          : null;

      destroyTerminal(tabId);
      createTerminal(tabId, {
        rebindPty: true,
        scrollbackSnapshot: snapshot,
        initialGeometry: { rows, cols },
      });

      if (previousSlot) {
        attachTerminal(tabId, previousSlot);
      }
    }, delay),
  );
}

/// Tear down a tab's terminal. Destroys xterm, unsubscribes every
/// listener, removes the host. Idempotent.
export function destroyTerminal(tabId: TabId): void {
  const entry = entries.get(tabId);
  if (!entry) return;
  entries.delete(tabId);
  // V39 Phase A: drop this tab's read-only notice timestamp with it.
  lastReadOnlyToastAt.delete(tabId);

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
  // pty-exit and tab-restart-requested listeners are module-scope and
  // dispatch via `entries.get(tabId)` — removing this entry from the
  // map (already done above via `entries.delete(tabId)`) is what
  // detaches them from this tab.
  entry.unsubFont();
  entry.unsubClosed?.();
  entry.unsubAppearance();
  entry.selectionListener?.dispose();
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
///
/// `options.focus` (default: false) controls whether the post-attach
/// rAF also calls `term.focus()`. The caller (typically Pane.svelte's
/// slot effect) passes `true` only for the *focused* pane — without
/// that gate every active-tab change in any pane would steal keyboard
/// focus from whichever pane the user is currently typing into. When
/// `focus` is false the host is still attached and fit; just no focus
/// shift.
export function attachTerminal(
  tabId: TabId,
  slot: HTMLElement,
  options: { focus?: boolean } = {},
): void {
  const entry = entries.get(tabId);
  if (!entry) return;
  if (entry.host.parentElement !== slot) {
    slot.appendChild(entry.host);
  }
  entry.attached = true;
  // M17: acquire the WebGL2 context now that this terminal is on screen.
  // Synchronous and in the same task as the DOM move on purpose — the
  // renderer swap (DomRenderer out, WebGL canvas in, `RenderService
  // .setRenderer` → `_fullRefresh`) is queued before the browser paints this
  // frame, so the user never sees an intermediate state. Deferring it to the
  // rAF below would expose one frame of bare host background. The addon
  // measures via CharSizeService (already measured at `term.open`), not host
  // layout, so it does not need the post-attach fit to have run — the fit
  // right below resizes the fresh renderer exactly as it did pre-M17.
  syncWebglRenderer(entry);
  const wantFocus = options.focus ?? false;
  requestAnimationFrame(() => {
    if (entries.get(tabId) !== entry) return;
    if (!entry.attached) return;
    fitAndResize(entry);
    if (wantFocus) entry.term.focus();
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
  // M17: release the WebGL2 context before the host leaves the slot, while it
  // still has real layout for the replacement DOM renderer to size against.
  // The repaint that `setRenderer` queues lands on the next frame — by then
  // the host is already offscreen, so the outgoing tab shows no flicker.
  // Reset the one-shot context-loss retry budget: the next time this terminal
  // is shown it starts a new visible session. The sticky `webglFailed` latch
  // is deliberately NOT reset — a machine without usable WebGL must not be
  // re-probed (and re-warned about) on every tab switch.
  entry.webglRetried = false;
  syncWebglRenderer(entry);
  const offscreen = ensureOffscreen();
  if (entry.host.parentElement !== offscreen) {
    offscreen.appendChild(entry.host);
  }
}

/// Trigger a manual respawn — invoked by `TabErrorOverlay`'s Retry
/// button. Uses pty_restart so any stale handle is shut down before
/// respawning.
export async function retryTerminal(tabId: TabId): Promise<void> {
  const entry = entries.get(tabId);
  if (!entry) return;
  await attemptSpawn(entry, 'restart');
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

/// The live xterm instance for `tabId`, if the registry is tracking one.
/// Backs V14 Phase A template variable substitution (`{selection}`), which
/// needs the FOCUSED pane's terminal specifically — unlike
/// `terminalWithSelection` below, which scans every terminal for one that
/// merely happens to have a non-empty selection.
export function getTerminal(tabId: TabId): Terminal | undefined {
  return entries.get(tabId)?.term;
}

/// The terminal that currently holds a non-empty selection, if any. Backs the
/// bottom-bar "play" transport (selection lives in whichever pane the user
/// last selected in, independent of which tab is "active"). xterm keeps its
/// selection model across focus changes, so clicking the toolbar button does
/// not clear it. Returns the first match in registry order — in practice only
/// one terminal has a selection at a time.
export function terminalWithSelection(): Terminal | undefined {
  for (const entry of entries.values()) {
    const sel = entry.term.getSelection();
    if (sel && sel.trim()) return entry.term;
  }
  return undefined;
}

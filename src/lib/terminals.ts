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
import { SerializeAddon } from '@xterm/addon-serialize';
import '@xterm/xterm/css/xterm.css';
import './terminals.css';
import { listen } from '@tauri-apps/api/event';
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
} from './terminal/background';
import { setTerminalFocuser } from './terminalFocus';
import { perTabClosedState } from './avatarState';
import { openConfigureTabDialog } from './dialog/store';
import { clearTabError, setTabError } from './tabs/errorState';
import { isShellTab, isOffloadTab, isGraphMonitorTab, type TabId } from './tabs/types';

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
    await listen<TabId>('tab-restart-requested', async (event) => {
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

function displayNameFor(t: TabId): string {
  if (t === 'claude') return 'Claude Code';
  if (t === 'claude-local') return 'Claude Code (local)';
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
  // V8-03/V9-01: the Offload Server and Code Graph monitor tabs render Svelte
  // dashboards instead of an xterm — no terminal entry for either.
  if (isOffloadTab(tabId) || isGraphMonitorTab(tabId)) return;

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
    // V1.4-03: on a renderer recreate, construct at the previous
    // terminal's geometry so the replayed scrollback's cursor positions
    // line up with the new grid. fit-on-attach corrects any minor
    // host-size differences after the snapshot lands.
    ...(options.initialGeometry ?? {}),
  });
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);
  // V1.4-03: serialize addon captures scrollback as ANSI for replay
  // across a renderer-flip recreate. Cheap to load even when no replay
  // ever happens.
  const serializeAddon = new SerializeAddon();
  term.loadAddon(serializeAddon);
  // V1.4-02: canvas renderer for the fast path (no image). Image mode
  // stays on the in-core DOM renderer — the canvas addon is a single
  // opaque surface and would obscure the CSS image beneath.
  if (initialCategory !== 'image') {
    term.loadAddon(new CanvasAddon());
  }
  term.open(host);

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
  //     must be recreated — the canvas addon is loaded once at
  //     construction and `allowTransparency` is constructor-only.
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
    void navigator.clipboard.writeText(text).catch((e) =>
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
    e.preventDefault();
    navigator.clipboard
      .readText()
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
    ptyWrite(tabId, data).catch((e) => console.error('pty_write failed:', e));
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
/// canvas addon and `allowTransparency` are construction-time
/// decisions. Live slider drags during a global edit can fire many
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

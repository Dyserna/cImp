/// V39 — the tab communication glyph, as VALUES.
///
/// Everything the glyph, its popover and the terminal's courtesy gate need to
/// decide lives here as pure functions, for the reason `latch.ts`'s
/// `protectionTint` and `status/sandboxChip.ts` give: a `.svelte` file has no
/// test harness in this repo, and a control that reports whether a tab is
/// accepting your keyboard must not be free to lie about it.
///
/// Phase A reaches only two of the glyph states (*off* and the *lock* overlay)
/// because nothing sets a role or starts a delegation yet. The whole table
/// from locked decision 7 is implemented anyway — it is the same function in
/// every phase, and a table half-written now is a table re-litigated later.
import type { Settings, TabConfig } from './settings/types';

/// A tab's delegation role (locked decision 8). Phase A always passes
/// `'none'`; the Role radio that can move it lands in Phase B.
export type DelegationRole = 'none' | 'manual' | 'remote';

/// Whether the *user's* keyboard reaches the tab. Not a statement about the
/// engine: a read-only tab is still a valid delegation worker — the lock
/// governs the user's hands, not cImp's.
export type TabAccess = 'rw' | 'ro';

/// The glyph's base shape. The read-only lock is an overlay on top of any of
/// them (`GlyphState.locked`), not a fifth state, because "this tab is a
/// worker" and "my keyboard is refused" are two independent facts and a single
/// enum would have to drop one of them.
export type GlyphBase = 'off' | 'manual' | 'remote' | 'driven';

export interface GlyphInput {
  role: DelegationRole;
  access: TabAccess;
  /// A delegation is in flight on this tab right now.
  inFlight: boolean;
  /// The driving tab's display name, for the in-flight attribution. Phase B
  /// supplies it; absent it the title says "another tab" rather than nothing.
  driverName?: string | null;
  /// The user-chosen backend name of a Remote-offload tab (Phase C).
  backendName?: string | null;
}

export interface GlyphState {
  state: GlyphBase;
  /// Render the lock overlay: the user's keyboard is refused.
  locked: boolean;
  /// The `title=` the glyph wears. Always says what the state is AND what a
  /// click does — the glyph is the only control surface for this (decision 7),
  /// so a tooltip that only names a state leaves the user with no next step.
  title: string;
}

/// The refusal reason the backend produces for a user lock. Spelled here as
/// well as in `state::manager::ReadOnlySource::reason` because the frontend
/// shows the same sentence *before* the round trip (the courtesy gate) as the
/// backend does after it — two different sentences for one refusal is how a
/// user learns to distrust both.
export const READ_ONLY_USER_REASON = 'read-only (user)';

/// The same, for the engine's lock. `by` is the driving tab's display name.
export function drivenReason(by: string): string {
  const who = by.trim();
  return `driven by ${who.length > 0 ? who : 'another tab'}`;
}

/// The full decision-7 table.
///
/// *Driven* wins while a delegation is in flight, whatever the role says: what
/// the tab is doing right now outranks what it is configured to be.
export function glyphState(input: GlyphInput): GlyphState {
  const locked = input.access === 'ro';
  const state: GlyphBase = input.inFlight
    ? 'driven'
    : input.role === 'manual'
      ? 'manual'
      : input.role === 'remote'
        ? 'remote'
        : 'off';

  const driver = (input.driverName ?? '').trim() || 'another tab';
  const backend = (input.backendName ?? '').trim();

  const base =
    state === 'driven'
      ? `cImp is using this tab — ${drivenReason(driver)}. Take over from the popover to stop waiting; no keys are ever sent to cancel it.`
      : state === 'manual'
        ? 'This tab is the delegation target for its harness (Role: Manual).'
        : state === 'remote'
          ? `This tab is a remote-offload worker${backend ? ` (backend "${backend}")` : ''}.`
          : 'This tab is not delegating.';

  const access =
    state === 'driven'
      ? // While driven the keyboard is refused by the engine's own lock, and
        // the user's sticky lock (if any) is not the interesting half.
        'Your keyboard is refused while the delegation runs.'
      : locked
        ? `Read-only: your keyboard is refused (${READ_ONLY_USER_REASON}). The tab keeps running — only your input is blocked.`
        : 'Read/write: this tab accepts your keyboard.';

  return { state, locked, title: `${base} ${access} Click to change access.` };
}

/// This tab's persisted access, read from the settings mirror.
///
/// Unknown tab ⇒ `'rw'`: a tab with no AI config is not lockable, and guessing
/// `'ro'` would put a lock glyph on a Shell tab.
export function accessOf(settings: Settings, tabId: string): TabAccess {
  return aiTabConfig(settings, tabId)?.read_only ? 'ro' : 'rw';
}

/// Whether the communication glyph belongs on this tab at all.
///
/// AI tabs only. Reserved dashboards are Shell-kind with no PTY, Shell and
/// Preview tabs are not harnesses, and none of them can be delegated to — a
/// control that does nothing is worse than no control.
export function hasCommIcon(settings: Settings, tabId: string): boolean {
  return aiTabConfig(settings, tabId) !== null;
}

/// The user-facing reason this tab is refusing input, or `null` when it is
/// not. Phase A knows only the persisted user lock; the engine's `Driven`
/// lock arrives with the engine.
export function readOnlyReason(settings: Settings, tabId: string): string | null {
  return accessOf(settings, tabId) === 'ro' ? READ_ONLY_USER_REASON : null;
}

/// Clone `settings` with one AI tab's `read_only` flag changed.
///
/// Used by the popover for its optimistic local update; the durable write goes
/// through the `tab_set_read_only` IPC, which also takes the runtime lock.
export function withTabReadOnly(settings: Settings, tabId: string, on: boolean): Settings {
  return {
    ...settings,
    tabs: settings.tabs.map((t) =>
      t.kind === 'ai_tool' && t.id === tabId ? { ...t, read_only: on } : t,
    ),
  };
}

/// **The terminal's own replies are not keystrokes.**
///
/// xterm answers the running program's queries — cursor-position reports,
/// device attributes, focus in/out — over the same `onData` channel the user
/// types on. A read-only tab must still answer them, or a TUI that asked where
/// the cursor is waits forever, which is exactly the state a delegation would
/// leave its worker in.
///
/// Mirrors `ipc::commands::is_automatic_terminal_response` byte for byte; the
/// two are asserted against the same fixtures on both sides.
export function isTerminalReply(data: string): boolean {
  if (data === '\x1b[I' || data === '\x1b[O') return true;
  if (data.length < 3) return false;
  if (data.charCodeAt(0) !== 0x1b || data[1] !== '[') return false;
  const last = data[data.length - 1];
  if (last !== 'R' && last !== 'c' && last !== 'n') return false;
  for (let i = 2; i < data.length - 1; i += 1) {
    const c = data[i];
    if (!((c >= '0' && c <= '9') || c === ';' || c === '?')) return false;
  }
  return true;
}

/// Whether `data` is *nothing but* mouse-wheel reports.
///
/// **Scrolling is reading.** A read-only tab exists so the user can watch it,
/// and in an alt-screen TUI the wheel is not local scrollback — xterm forwards
/// it to the program as a mouse report, so a swallowed wheel leaves a tab one
/// may watch but not scroll. Mouse *clicks* stay refused: a click activates
/// whatever control is under it (a permission option, for one), which is the
/// input the lock exists for. Drag goes with the click — a drag is a held
/// button.
///
/// Whole-input and repeat-until-exhausted, never "starts with": a wheel report
/// followed by typed text is refused, so the exemption cannot smuggle a
/// keystroke past the lock. Repeats pass because a fast scroll can arrive as
/// several reports in one chunk.
///
/// Mirrors `ipc::commands::is_mouse_wheel`; both sides assert the same
/// fixture table.
export function isMouseWheel(data: string): boolean {
  let rest = data;
  let seen = false;
  while (rest.length > 0) {
    const next = takeWheelReport(rest);
    if (next === null) return false;
    seen = true;
    rest = next;
  }
  return seen;
}

/// Everything the read-only lock lets through: the terminal answering the
/// running program, and the wheel. This is what the courtesy gate asks —
/// `isTerminalReply` keeps its own narrower meaning.
export function readOnlyExempt(data: string): boolean {
  return isTerminalReply(data) || isMouseWheel(data);
}

/// Consume one leading wheel report, returning what follows it, or `null`.
///
/// Both encodings xterm can emit: SGR (`ESC [ < Cb ; Cx ; Cy M`) and the
/// legacy X10/normal one (`ESC [ M` + three code points, each offset by 32).
/// SGR wheel reports end in `M` only — xterm emits no release for a wheel, so
/// a `…m` form is treated like any other click release and refused.
function takeWheelReport(s: string): string | null {
  if (s.startsWith('\x1b[<')) {
    const body = s.slice(3);
    const end = body.indexOf('M');
    if (end < 0) return null;
    const params = body.slice(0, end).split(';');
    if (params.length !== 3) return null;
    if (!params.every((p) => /^\d+$/.test(p))) return null;
    return isWheelButton(Number(params[0])) ? body.slice(end + 1) : null;
  }
  if (s.startsWith('\x1b[M')) {
    // Code points, not UTF-16 units: xterm's UTF-8 extended coordinates can
    // exceed 127, and splitting one in half would misread the next report.
    const rest = [...s.slice(3)];
    if (rest.length < 3) return null;
    const cb = (rest[0].codePointAt(0) ?? 0) - 32;
    if (cb < 0) return null;
    return isWheelButton(cb) ? rest.slice(3).join('') : null;
  }
  return null;
}

/// Bit 64 = wheel (64/65 vertical, 66/67 horizontal); bit 32 = motion, which a
/// wheel never sets and a drag always does; modifier bits (shift 4, meta 8,
/// ctrl 16) may be set — ctrl+wheel is still a wheel.
function isWheelButton(cb: number): boolean {
  return Number.isInteger(cb) && cb >= 0 && cb < 128 && (cb & 0b110_0000) === 0b100_0000;
}

/// Recognize the backend's read-only refusal in a rejected `pty_write`.
///
/// `AppError` serializes to its `Display` string, so this matches the shape
/// `AppError::ReadOnly` produces — ``tab `claude` is read-only (user)`` — and
/// returns the whole sentence for the toast. Anything else returns `null` and
/// keeps the existing console-error path: a refusal must be surfaced, but an
/// unrelated PTY failure must not be mislabelled as one.
export function readOnlyRefusalMessage(error: unknown): string | null {
  const text = typeof error === 'string' ? error : error instanceof Error ? error.message : '';
  return /\bis (read-only \(|driven by )/.test(text) ? text : null;
}

function aiTabConfig(
  settings: Settings,
  tabId: string,
): (TabConfig & { kind: 'ai_tool' }) | null {
  const found = settings.tabs.find((t) => t.id === tabId);
  return found && found.kind === 'ai_tool' ? found : null;
}

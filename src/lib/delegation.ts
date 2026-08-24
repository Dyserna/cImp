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
import type {
  AiToolTabConfig,
  BackendTier,
  DelegationBackend,
  DelegationRole,
  Settings,
  TabConfig,
} from './settings/types';
import { attributionLine, harnessForCommand, harnessLabel } from './harness';

/// A tab's delegation role (locked decision 8) — re-exported from the settings
/// mirror rather than spelled again here.
///
/// **Phase B corrects Phase A's spelling.** Phase A declared its own
/// `'none' | 'manual' | 'remote'` because nothing persisted a role yet; the
/// persisted field's serde is `snake_case` over Rust's `RemoteOffload`, so the
/// wire word is `remote_offload`. One spelling, and it is the wire's — a UI
/// enum that disagrees with the field it is bound to is a bug that only shows
/// up on the one path nobody tests by hand.
export type { DelegationRole };

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
  /// The driving tab's HARNESS id (`InFlightView.driver_agent`), which is what
  /// the attribution line names first — "delegated by <that harness>" is the fact the
  /// user needs; the tab name only disambiguates which of that harness's tabs.
  driverAgent?: string | null;
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
      : input.role === 'remote_offload'
        ? 'remote'
        : 'off';

  const backend = (input.backendName ?? '').trim();

  const base =
    state === 'driven'
      ? // Locked decision 2a: the glyph title REPEATS the attribution line, so
        // the banner, the local echo and this tooltip say one sentence rather
        // than three paraphrases of it.
        `${attributionLine(input.driverAgent, input.driverName)} cImp is using this tab. Take over from the popover to stop waiting; no keys are ever sent to cancel it.`
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

/// **What the terminal's courtesy gate should refuse** (V39 review R-4).
///
/// `readOnlyReason` answers from the PERSISTED lock alone, which is right for
/// the glyph and wrong for the keyboard: locked decision 5 opens the keyboard
/// while the worker holds a prompt, because the user's answer is the only
/// thing that lets that turn finish. The backend implements exactly that
/// (`ReadOnlyEntry::prompt_relaxed`), but the gate ran first and swallowed the
/// keystroke before `pty_write` was ever called — so on a tab the user had also
/// locked by hand, the prompt could not be answered and the delegation ran to
/// its deadline reporting "worker awaiting permission".
///
/// `promptRelaxed` comes from the in-flight mirror (`delegationPrompt.ts`).
/// When `delegation.auto_read_only` is off there is no engine lock to relax and
/// a user lock still refuses — server-side, with its own reason — which is the
/// honest answer for a tab whose owner locked it and turned the relaxation off.
export function courtesyRefusal(
  settings: Settings,
  tabId: string,
  promptRelaxed: boolean,
): string | null {
  if (promptRelaxed) return null;
  return readOnlyReason(settings, tabId);
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
/// `AppError::ReadOnly` produces — ``tab `<id>` is read-only (user)`` — and
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

// ── V39 Phase B: roles, harnesses, attribution ──────────────────────────────

/// The in-flight delegation on one worker tab. Mirror of Rust
/// `delegation::InFlightView` (plain `serde::Serialize`, so the field names are
/// the Rust ones verbatim).
export interface InFlightView {
  /// Driver tab id.
  driver: string;
  /// Driver tab display name, snapshotted when the flight started — it keeps
  /// naming the tab the user saw even if that tab is renamed or closed.
  driver_name: string;
  /// Driver HARNESS id — a registry id, as `harness_list` publishes it.
  driver_agent: string;
  mode: 'explicit' | 'facade';
  started_ms: number;
  /// A permission/question prompt is standing on the worker right now: the
  /// keyboard is relaxed for it and the deadline has one bounded extension
  /// (locked decision 5).
  awaiting_prompt: boolean;
}

/// The `delegation-changed` payload: **the whole in-flight set, every time**.
/// A snapshot, never a delta — the store REPLACES its state with it.
export interface DelegationChanged {
  in_flight: [string, InFlightView][];
}

/// What `tab_set_delegation_role` did, for the toast. Mirror of Rust
/// `service::delegation::RoleChange` (re-exported as `ipc::commands::RoleChange`,
/// which is the command's return type). `displaced` is the tab that LOST Manual to this
/// call — `null` when nothing moved.
export interface RoleChange {
  tab: string;
  role: DelegationRole;
  displaced: string | null;
}

/// The display name for a harness id, and V39's attribution line — both from
/// the registry now (V40 Phase F, locked decision 27 / amendment 0-d).
///
/// V39 added a `HARNESS_LABELS` map and a hand-written template here, which was
/// the right call then (nothing else in the frontend held either) and is a
/// second declaration of the roster now. `harness.ts` reads both off
/// `harness_list`: the label is the descriptor's, and the line comes from the
/// driver harness's declared `attributionTemplate`, so the banner, the local
/// echo and the glyph title still share ONE source — it just is not this file.
///
/// Re-exported rather than moved-and-repointed so the V39 call sites (banner,
/// popover, chip, local echo) keep reading them from the delegation module they
/// belong to.
export { harnessLabel, attributionLine };

/// Elapsed time for the banner, as a short human string. Seconds under a
/// minute, `Nm SSs` above it — a delegation that has been running for four
/// minutes must not read as "247s", which nobody parses at a glance.
export function elapsedLabel(startedMs: number, nowMs: number): string {
  const secs = Math.max(0, Math.floor((nowMs - startedMs) / 1000));
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}m ${String(s).padStart(2, '0')}s`;
}

/// The minimum of an xterm `Terminal` the local echo needs. A structural type
/// so the helper is testable with a two-line fake, and so it can never reach
/// for anything else on the real object.
export interface LocalEchoTarget {
  writeln(data: string): void;
}

/// Write the attribution line into a tab's terminal WIDGET (locked decision
/// 2a).
///
/// This is display, not input: it goes to the xterm instance the user is
/// looking at, never through `pty_write` and never into the backend scrollback
/// ring, so no harness — the worker's least of all — ever sees it. Styled dim
/// italic with SGR so it cannot be mistaken for the harness's own output, and
/// reset immediately so the sequence cannot leak into the next line the PTY
/// writes.
///
/// **Accepted residual:** a local echo does NOT survive a scrollback re-seed.
/// Rebinding a tab (a renderer flip, a restart) repaints from the backend ring,
/// which by construction never held this line — so it disappears. The banner
/// holds for the whole flight and the Events row is the durable record; this
/// line's job is to mark the spot in the visible transcript where the turn
/// began.
export function writeLocalEcho(
  term: LocalEchoTarget,
  driverAgent: string | null | undefined,
  driverName: string | null | undefined,
): void {
  term.writeln(`\r\n\x1b[2;3m${attributionLine(driverAgent, driverName)}\x1b[0m`);
}

/// The follow-up sentence a read-only refusal toast carries.
///
/// **The two locks do not end the same way, so they must not give the same
/// advice.** A user lock is lifted by the Access radio in the ⇄ popover. The
/// ENGINE's lock is not: a radio never lifts a lock a delegation owns (locked
/// decisions 4 and 6) — that radio is disabled for the whole flight, and "Take
/// over" is the control that ends it. Sending a driven tab's owner to the radio
/// would point them at a disabled control and say nothing about the flight they
/// are actually waiting on.
///
/// Keyed on the refusal MESSAGE (the backend's own sentence, which names the
/// driving tab) rather than on a second lookup, so the advice cannot disagree
/// with the reason it is appended to.
export function readOnlyAdvice(message: string): string {
  return /\bdriven by /.test(message)
    ? 'Take over from the tab’s ⇄ popover or its context menu — the worker is never sent any keys.'
    : 'Use the ⇄ icon on the tab to allow input again.';
}

/// This tab's persisted delegation role, read from the settings mirror.
/// A tab with no AI config has no role — `'none'`, never a guess.
export function roleOf(settings: Settings, tabId: string): DelegationRole {
  return aiTabConfig(settings, tabId)?.delegation_role ?? 'none';
}

/// This tab's persisted facade-backend knobs, or the defaults.
export function backendOf(settings: Settings, tabId: string): DelegationBackend {
  return (
    aiTabConfig(settings, tabId)?.delegation_backend ?? {
      name: null,
      tier: 'quality',
      declared_context: null,
    }
  );
}

/// Which HARNESS a configured AI tab runs, or `null` for one that runs neither.
///
/// The frontend needs it because "at most one Manual tab per harness" is a rule
/// ABOUT this grouping, and the popover has to name the tab that currently
/// holds it before the user clicks.
///
/// **V40 Phase F (locked decision 2, frontend half).** This used to be a second
/// hand-written classifier whose `else` branch made every unrecognised command
/// one particular harness — so a tab running a harness this build does not know
/// was not rejected, it was MISATTRIBUTED: eligible for another harness's
/// Manual slot, and typed into with that harness's paste rules. It resolves through the registry's declared binaries now and answers
/// `null` for a command nobody declared, exactly as Rust's `tab_consumer` does
/// after Phase A — the two must agree, or the popover names a tab the backend
/// would not have displaced.
export function tabHarness(cfg: AiToolTabConfig): string | null {
  return harnessForCommand(cfg.command ?? '')?.id ?? null;
}

/// The tab that currently holds `Manual` for `tabId`'s harness, other than
/// `tabId` itself — `null` when this tab holds it, or nobody does.
///
/// Read from the settings mirror rather than from the backend, because it is
/// wanted BEFORE the click: the popover says which tab holds Manual so the user
/// knows the radio will MOVE it rather than be refused.
export function manualHolderFor(
  settings: Settings,
  tabId: string,
): { id: string; name: string } | null {
  const self = aiTabConfig(settings, tabId);
  if (!self) return null;
  const harness = tabHarness(self);
  // A tab running no registered harness holds no harness's Manual slot, so
  // there is nobody to displace and nothing to name.
  if (harness === null) return null;
  for (const t of settings.tabs) {
    if (t.kind !== 'ai_tool' || t.id === tabId) continue;
    if (t.delegation_role === 'manual' && tabHarness(t) === harness) {
      return { id: t.id, name: t.name };
    }
  }
  return null;
}

/// The toast the DISPLACED tab gets when Manual moves off it (locked decision
/// 8): the losing tab may not even be visible, and a role that moved silently
/// is a `delegate_task_*` tool that started driving somewhere else with nothing
/// on screen saying so.
export function displacedToast(
  displacedName: string,
  harness: string,
  takerName: string,
): string {
  return `“${displacedName}” is no longer the Manual ${harnessLabel(harness)} tab — moved to “${takerName}”.`;
}

/// **The backend name a Remote-offload tab takes when the user picks none**
/// (V39 review L-2) — the mirror of Rust `settings::facade_default_name`.
///
/// The default used to be the tab's display name, which is rendered into
/// `offload_task`'s description and into the driver's own result: the asking
/// model could read off what its "LAN backend" really was. `worker-<4 hex>` of
/// a hash of the tab ID is stable across renames and says nothing.
///
/// FNV-1a over the id's UTF-8 bytes, byte for byte with the Rust side —
/// `TextEncoder`, not `charCodeAt`, so a non-ASCII id hashes the same on both.
export function defaultFacadeName(tabId: string): string {
  let h = 0x811c9dc5;
  for (const byte of new TextEncoder().encode(tabId)) {
    h ^= byte;
    h = Math.imul(h, 0x01000193);
  }
  const top = (h >>> 16) & 0xffff;
  return `worker-${top.toString(16).padStart(4, '0')}`;
}

/// One synthesized facade backend, as the Settings window lists it (V39 Phase
/// C). The mirror of what Rust's `Settings::effective_offload_backends`
/// appends — read-only here, because the entry does not exist in
/// `offload.backends` and saving one would be saving a view.
export interface FacadeBackendRow {
  /// The worker tab's id, for the "configured on the tab" link.
  tabId: string;
  /// The tab's display name — shown as the SOURCE of the row, never as the
  /// backend name a driver sees.
  tabName: string;
  /// The name the requesting harness sees: the tab's chosen backend name, or
  /// [`defaultFacadeName`] when it has none. The same fallback Rust applies.
  name: string;
  tier: BackendTier;
  declaredContext: number | null;
  /// Whether the offload pool actually contains this row.
  ///
  /// A facade whose name is already taken is **dropped** by
  /// `Settings::effective_offload_backends` rather than renamed — the router,
  /// the run log and the dashboard all key on the name, and two entries
  /// answering to one name is a bug with no good half. The drop was a `warn!`
  /// nobody reads, so before V39 review M-9 this list showed the row as if it
  /// were live and the user had a backend the router would never pick.
  inPool: boolean;
  /// Why it is not in the pool, for the row to render. `null` when it is.
  droppedReason: string | null;
}

/// The sentence a dropped facade row wears. Spelled once: it is the only place
/// the user is ever told, and the whole point of M-9 is that it IS said.
export const FACADE_NAME_TAKEN =
  'not in the pool \u2014 the name is taken by another backend. Rename it in this tab\u2019s \u21c4 popover.';

/// Every Remote-offload tab, as the backend the offload pool synthesizes from
/// it.
///
/// **Deliberately a re-derivation, not a fetch.** The list is a pure function
/// of settings the window already holds, so it cannot be stale relative to the
/// role radio in the same window; an IPC round-trip could. The fallback rule
/// (blank backend name ⇒ the tab name) is mirrored from Rust rather than
/// inferred, and `facade_rows_mirror_the_rust_fallback` is what keeps the two
/// honest.
export function facadeBackends(settings: Settings): FacadeBackendRow[] {
  // The names already spoken for, in the order Rust builds the pool:
  // `OffloadSettings::effective_backends` first (the configured list, or the
  // one synthesized `local` entry when there is none), then each facade as it
  // is appended — so a second facade answering to the first one's name is
  // dropped too, exactly as it is in `effective_offload_backends`.
  const taken = new Set<string>(
    settings.offload.backends.length > 0
      ? settings.offload.backends.map((b) => b.name)
      : ['local'],
  );
  const rows: FacadeBackendRow[] = [];
  for (const t of settings.tabs) {
    if (t.kind !== 'ai_tool' || t.delegation_role !== 'remote_offload') continue;
    const chosen = (t.delegation_backend?.name ?? '').trim();
    const name = chosen || defaultFacadeName(t.id);
    const inPool = !taken.has(name);
    if (inPool) taken.add(name);
    rows.push({
      tabId: t.id,
      tabName: t.name,
      name,
      tier: t.delegation_backend?.tier ?? 'quality',
      declaredContext: t.delegation_backend?.declared_context ?? null,
      inPool,
      droppedReason: inPool ? null : FACADE_NAME_TAKEN,
    });
  }
  return rows;
}

// V39 review M-10: `withTabBackend` — the clone-and-save helper the popover
// used to build a whole-document `applySettings` from — is GONE, not deprecated.
// The knobs now go through `tab_set_delegation_backend`, a command that writes
// three fields under `settings.mutate`. Keeping a helper that produces a
// whole-document save is keeping the lost-update shape one import away.

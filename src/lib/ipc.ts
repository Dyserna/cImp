import { invoke, Channel } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { TabId, TabKind } from './tabs/types';
import type { DelegationRole, InFlightView, RoleChange } from './delegation';

export type BytesChannel = Channel<string>;

export function createBytesChannel(): BytesChannel {
  return new Channel<string>();
}

/// V1.4-04 D.5: `pty_start` now returns the persisted-scrollback bytes
/// (if any) from the previous session. The caller writes them to the
/// new xterm before binding the live channel so the user sees their
/// last shell output above a fresh prompt. Returns `null` when:
///   - `terminal.scrollback.restore_on_launch` is `false`
///   - no persisted file exists (cold install, or already consumed
///     earlier in this session)
///   - the persisted-file read failed (logged backend-side; the
///     spawn proceeds either way)
///
/// Tauri serializes Rust `Vec<u8>` as `number[]`. Callers convert to
/// `Uint8Array` + `TextDecoder` to feed `term.write`.
export async function ptyStart(
  tab: TabId,
  channel: BytesChannel,
  rows: number,
  cols: number,
): Promise<number[] | null> {
  return invoke<number[] | null>('pty_start', { tab, channel, rows, cols });
}

export async function ptyRestart(
  tab: TabId,
  channel: BytesChannel,
  rows: number,
  cols: number
): Promise<void> {
  await invoke('pty_restart', { tab, channel, rows, cols });
}

/// V1.4-03: re-point a still-running PTY's bytes at a fresh channel.
/// Used by the renderer-flip recreate path so the shell session, env,
/// cwd, and running processes survive the xterm.js destroy/create
/// cycle. Errors when the PTY isn't registered or has already exited
/// — the caller handles the fallback to `ptyStart`.
export async function ptyRebindChannel(
  tab: TabId,
  channel: BytesChannel,
): Promise<void> {
  await invoke('pty_rebind_channel', { tab, channel });
}

export async function ptyWrite(tab: TabId, input: string): Promise<void> {
  await invoke('pty_write', { tab, input });
}

/// V39 Phase A: set or clear a tab's user read-only lock. Takes the runtime
/// lock in the backend AND persists the flag, so the tab is refusing input by
/// the time this resolves and is still refusing it after a restart. The new
/// state reaches this window as a `settings-changed` broadcast — there is no
/// separate event to listen for.
export async function tabSetReadOnly(tab: TabId, on: boolean): Promise<void> {
  await invoke('tab_set_read_only', { tab, on });
}

/// V39 Phase B (locked decision 8): set a tab's delegation role.
///
/// The backend enforces **at most one Manual tab per harness** and MOVES the
/// role rather than refusing, so the answer carries `displaced` — the id of the
/// tab that lost it, or `null`. The caller toasts on that tab: the loser may
/// not be visible, and a role that moved silently is a `delegate_task_*` tool
/// that started driving somewhere else with nothing on screen saying so.
///
/// Persists the ROLE only; the Remote-offload knobs beside it ride the ordinary
/// settings save. Refuses — naming the condition — on a reserved dashboard, a
/// non-AI tab, and a harness with no input profile.
export async function tabSetDelegationRole(
  tab: TabId,
  role: DelegationRole,
): Promise<RoleChange> {
  return invoke<RoleChange>('tab_set_delegation_role', { tab, role });
}

/// V39 Phase B (locked decision 6): take a driven tab back.
///
/// Stops the driver waiting and clears the engine's lock. Sends the worker
/// NOTHING — no Escape, no interrupt; it finishes its turn visibly. Returns
/// whether a delegation was actually in flight, so the UI can tell "I cancelled
/// it" from "it had already finished".
export async function delegationTakeOver(tab: TabId): Promise<boolean> {
  return invoke<boolean>('delegation_take_over', { tab });
}

/// V39 Phase B: what is driving `tab` right now, if anything.
export async function delegationStatus(tab: TabId): Promise<InFlightView | null> {
  return invoke<InFlightView | null>('delegation_status', { tab });
}

/// V39 Phase B: every in-flight delegation, keyed by worker tab id — the pull
/// that pairs with the `delegation-changed` push, for a window that mounts
/// mid-flight.
export async function delegationStatuses(): Promise<[string, InFlightView][]> {
  return invoke<[string, InFlightView][]>('delegation_statuses');
}

export async function ptyResize(tab: TabId, rows: number, cols: number): Promise<void> {
  await invoke('pty_resize', { tab, rows, cols });
}

export async function ttsTest(text: string): Promise<void> {
  await invoke('tts_test', { text });
}

/// Read arbitrary text aloud through the TTS worker (skips the
/// processor, routed to the active tab). Backs the Ctrl+right-click
/// "speak selection" gesture.
export async function ttsSpeak(text: string): Promise<void> {
  await invoke('tts_speak', { text });
}

/// Read a terminal selection aloud as a read-along. `chunks` are the
/// pre-split sentence segments (so the spoken text matches the highlight
/// exactly); `session` is a frontend-assigned monotonic id used to correlate
/// `tts-selection-progress` events and to let `ttsStop` cancel this read.
export async function ttsSpeakSelection(
  session: number,
  chunks: string[],
): Promise<void> {
  await invoke('tts_speak_selection', { session, chunks });
}

/// Stop all TTS playback and cancel any in-flight selection read. Backs the
/// Esc gesture that also clears the read-along highlight.
export async function ttsStop(): Promise<void> {
  await invoke('tts_stop');
}

/// Pause (`true`) or resume (`false`) TTS playback without discarding queued
/// audio. Backs the bottom-bar selection-TTS pause/resume transport.
export async function ttsSetPaused(paused: boolean): Promise<void> {
  await invoke('tts_set_paused', { paused });
}

/// One Claude usage quota window. `utilization` is 0–100; `resets_at` is an
/// ISO-8601 timestamp (with timezone) or null.
export interface UsageWindow {
  utilization: number;
  resets_at: string | null;
}

/// Live context-window reading (NC-3), pulled from the same status-line push
/// as the quota windows. Every field is independently optional: `null` means
/// "not reported" and must render as unknown — never as 0.
export interface ContextSnapshot {
  /// Percentage of the context window in use (0–100).
  used_percentage?: number | null;
  /// Percentage still free (0–100), as reported (not derived).
  remaining_percentage?: number | null;
  /// Tokens occupying the window (input + cache).
  total_input_tokens?: number | null;
  /// Window size in tokens (200k, or 1M with extended context).
  context_window_size?: number | null;
  /// Latest turn's tokens served from the prompt cache.
  cache_read_tokens?: number | null;
  /// Latest turn's tokens written into the prompt cache.
  cache_creation_tokens?: number | null;
  /// Latest turn's uncached input tokens.
  input_tokens?: number | null;
  /// Latest turn's output tokens.
  output_tokens?: number | null;
  /// Session name, when Claude Code names the session.
  session_name?: string | null;
  /// Active agent/persona (`agent.name`).
  agent_name?: string | null;
  /// Reasoning effort as reported (free-form).
  effort?: string | null;
  /// Thinking setting as reported ('on'/'off' for the boolean form).
  thinking?: string | null;
  /// Fast-mode flag.
  fast_mode?: boolean | null;
}

/// Session (5h) + weekly (7d) quota plus the live context reading for the
/// bottom-bar tracker. Each part is independently absent-able.
export interface UsageSnapshot {
  five_hour: UsageWindow | null;
  seven_day: UsageWindow | null;
  /// NC-3 context reading; absent on older pushes / older Claude Code.
  context?: ContextSnapshot | null;
}

/// Outcome of a usage read. Data is pushed by the Claude tab's status line
/// (`cimp --statusline` persists the payload's `rate_limits`) and read back
/// from disk — no network involved:
///   - `snapshot` set, `stale` false → something on it is a fresh push from a
///     live Claude tab.
///   - `snapshot` set, `stale` true → every part is aging (tabs closed or gone
///     quiet); render dimmed.
///   - all empty → no push data (no Claude tab has reported yet, or the last
///     push expired) — hide the widget.
///   - `rate_limited` / `retry_after_secs` are legacy fields from the retired
///     endpoint-poll path (kept in the shape; always false / null now).
///
/// M14: the quota and context halves are written by different Claude tabs and
/// age on their own clocks, so each carries its own staleness flag; `stale` is
/// the whole-widget roll-up (true only when nothing on screen is fresh).
export interface UsageResult {
  snapshot: UsageSnapshot | null;
  rate_limited: boolean;
  retry_after_secs: number | null;
  stale: boolean;
  /// Quota half present but aging. False when there is no quota data at all.
  quota_stale: boolean;
  /// Context half present but aging. False when there is no context data.
  context_stale: boolean;
}

/// Fetch the current Claude Code usage. See `UsageResult` for the outcomes.
export async function getClaudeUsage(): Promise<UsageResult> {
  return invoke<UsageResult>('get_claude_usage');
}

/// NVIDIA GPU stats (null when no NVIDIA GPU / NVML).
export interface GpuStats {
  util_pct: number;
  mem_pct: number;
  temp_c: number;
}

/// Network throughput, bytes/sec, since the previous sample.
export interface NetStats {
  down_bps: number;
  up_bps: number;
}

/// One system-monitor sample (CPU / memory / GPU / network).
export interface SystemStatsSnapshot {
  cpu_pct: number;
  mem_pct: number;
  gpu: GpuStats | null;
  net: NetStats;
}

/// Sample the system-monitor stats for the bottom-bar panel.
export async function getSystemStats(): Promise<SystemStatsSnapshot> {
  return invoke<SystemStatsSnapshot>('get_system_stats');
}

export async function composeContentChanged(nonEmpty: boolean): Promise<void> {
  await invoke('compose_content_changed', { nonEmpty });
}

export async function acknowledgeError(tab: TabId): Promise<void> {
  await invoke('acknowledge_error', { tab });
}

export interface TabMetaWire {
  id: TabId;
  kind: TabKind;
  name: string;
  builtin: boolean;
}

/// Snapshot the live tab list. Called once from `App.svelte`'s onMount to
/// seed the tabs store deterministically; runtime add/remove arrives via
/// `tab-created`/`tab-closed` events afterward.
export async function listTabs(): Promise<TabMetaWire[]> {
  return invoke<TabMetaWire[]>('list_tabs');
}

/// Wire shape of the backend's `TabLifecycleError`. Internally tagged on
/// `kind`; struct variants flatten their fields alongside.
export type TabLifecycleError =
  | { kind: 'empty-name' }
  | { kind: 'command-not-found'; tried: string }
  | { kind: 'cwd-not-found'; path: string }
  | { kind: 'tab-not-found'; tab: TabId }
  | { kind: 'builtin-not-closable' }
  | { kind: 'wrong-kind' }
  | { kind: 'spawn-failed'; message: string }
  | { kind: 'internal'; message: string };

/// Default shell + args returned by `default_shell_spec`. Args are
/// pre-joined with spaces; the dialog drops them into a text input
/// verbatim and the backend re-splits via `shlex` on submit. The two
/// `notifications_*` fields carry the platform-default notification
/// text so the New Shell Tab dialog can pre-fill them — keeping the
/// defaults source-of-truth on the backend (M4).
export interface DefaultShellWire {
  command: string;
  args: string;
  git_bash_found: boolean;
  notifications_error: string;
  notifications_exited: string;
}

export async function defaultShellSpec(): Promise<DefaultShellWire> {
  return invoke<DefaultShellWire>('default_shell_spec');
}

export interface ShellTabConfigWire {
  name: string;
  command: string;
  args: string;
  cwd: string | null;
  env: Record<string, string>;
  notifications_error: string;
  notifications_exited: string;
}

export async function getShellTabConfig(tab: TabId): Promise<ShellTabConfigWire> {
  return invoke<ShellTabConfigWire>('get_shell_tab_config', { tab });
}

// Tauri v2 converts Rust snake_case parameter names to camelCase on the
// JS side. The `argsString` field below maps to the backend's
// `args_string: String` parameter; the dialog still sends raw shell-style
// strings and the backend re-splits via `shlex` on receive.
export interface CreateShellTabInput {
  name: string;
  command: string;
  argsString: string;
  cwd: string | null;
  env: Record<string, string>;
  notificationsError: string;
  notificationsExited: string;
}

export async function createShellTab(input: CreateShellTabInput): Promise<TabId> {
  return invoke<TabId>('create_shell_tab', input as unknown as Record<string, unknown>);
}

/// Spawn a duplicate of an existing AI tab (the `+` on a Claude/OpenCode
/// builtin). `template` is the id of the tab to clone; the backend copies
/// its live config, assigns a fresh `ai-<uuid>` id, and returns the new
/// tab id. The new tab is closable (`builtin: false`) and persists across
/// restarts.
export async function createAiTab(template: TabId): Promise<TabId> {
  return invoke<TabId>('create_ai_tab', { template });
}

/// V13 Phase D D3: "New <Claude|OpenCode> tab in worktree…" — creates a
/// fresh cImp worktree (`.cimp/worktrees/<slug>`, branch `cimp/<slug>` cut
/// from `HEAD`) then spawns a duplicate of `template`'s config with `cwd`
/// pointed at it. Throws the same `TabLifecycleError` shape as
/// `createAiTab` on a tab-registration failure; a worktree-creation failure
/// (nested repo, detached HEAD, duplicate slug, ...) throws a plain string
/// (the backend's `AppError::Workbench` message).
export async function createAiTabInWorktree(
  template: TabId,
  slug: string,
  root?: string,
): Promise<TabId> {
  return invoke<TabId>('create_ai_tab_in_worktree', { template, slug, root: root ?? null });
}

export async function closeTab(tab: TabId): Promise<void> {
  await invoke('close_tab', { tab });
}

export async function renameTab(tab: TabId, newName: string): Promise<void> {
  await invoke('rename_tab', { tab, newName });
}

export interface ReconfigureShellTabInput {
  tab: TabId;
  name: string;
  command: string;
  argsString: string;
  cwd: string | null;
  env: Record<string, string>;
  notificationsError: string;
  notificationsExited: string;
  /// V1.4-01 per-tab terminal palette override. `null` inherits the
  /// global `terminal.theme`. The backend stamps it onto
  /// `tabs[].theme_override` in the same write that updates command/args.
  themeOverride: import('./settings/types').TerminalThemeSettings | null;
  /// V1.4-03 per-tab terminal background override. Three-state:
  ///   `null`        → inherit global `terminal.background`
  ///   `'disabled'`  → opt this tab out, even if global has an image/color
  ///   `{...config}` → use this config for this tab specifically
  /// Stamped onto `tabs[].background_override` in the same write.
  backgroundOverride:
    | import('./settings/types').BackgroundOverrideWire
    | null;
}

export async function reconfigureShellTab(input: ReconfigureShellTabInput): Promise<void> {
  await invoke('reconfigure_shell_tab', input as unknown as Record<string, unknown>);
}

/// Restart a closed Shell tab. Backend validates the tab kind/state and
/// emits `tab-restart-requested` so the terminal registry rebinds its
/// bytes channel via `pty_restart`. The `TabClosedStateChanged
/// { closed: false }` event clears the overlay once the new PTY has
/// spawned.
export async function restartShellTab(tab: TabId): Promise<void> {
  await invoke('restart_shell_tab', { tab });
}

/// Apply a new `enabled_ai_tabs` value: opens / closes the AI builtin
/// tabs as needed. The backend kills the PTY and drops scrollback for
/// any newly-disabled tab; newly-enabled tabs spawn fresh on the next
/// frontend mount. Switches the active tab off any soon-to-be-removed
/// tab onto a surviving AI tab so the avatar/TTS gate doesn't dangle.
/// Empty `value` is rejected — the user must keep at least one AI tab
/// enabled.
export async function setEnabledAiTabs(
  value: import('./settings/types').AiTabId[],
): Promise<void> {
  await invoke('set_enabled_ai_tabs', { value });
}

/// A built-in tool launchable from the bottom-bar quick-launch buttons (V16).
export type ToolKind = 'rustnet' | 'broot';

/// Launch a built-in tool (rustnet / broot) into a fresh closable Shell tab
/// (V16). The backend spawns a new uuid-id Shell tab running the tool's fixed
/// command, lands it in the focused pane, and activates it — each call opens
/// another tab, so the user can run as many as they like and close them
/// individually. A missing tool still opens the tab and shows the standard
/// "command not found" overlay. Returns the new tab id.
export async function openToolTab(tool: ToolKind): Promise<TabId> {
  return invoke<TabId>('open_tool_tab', { tool });
}

/// Open the Note scratchpad tab (bottom-bar button). Singleton: re-activates
/// the tab if it's already open, otherwise creates it (and its backing
/// `.cimp/cimp.note.txt` file) and activates it. Returns the note tab id.
export async function openNoteTab(): Promise<TabId> {
  return invoke<TabId>('open_note_tab');
}

/// Load the note's text, creating an empty `.cimp/cimp.note.txt` on first open.
export async function readNote(): Promise<string> {
  return invoke<string>('read_note');
}

/// Persist the note's text (atomic write into `.cimp/cimp.note.txt`). Called by
/// the NoteView autosave (debounced on edit, on a 5s timer, and on close).
export async function writeNote(content: string): Promise<void> {
  await invoke('write_note', { content });
}

// ── V14 Phase F: Preview tab ─────────────────────────────────────────────

/// Create a new user-managed Preview tab (the toolbar's "New Preview tab"
/// affordance). An empty `url` falls back to `Settings.preview_last_url`,
/// then `lib/preview/policy.ts`'s `DEFAULT_PREVIEW_URL`, on the backend.
export async function createPreviewTab(url: string): Promise<TabId> {
  return invoke<TabId>('create_preview_tab', { url });
}

/// Open (or replace, if already open) the child webview for `tab`'s
/// Preview pane at `url`, positioned at `rect` (logical/CSS pixels, relative
/// to the main window's content area). Called once from the pane body's
/// `onMount`. Rejects `url` against the live navigation policy before ever
/// touching a webview — throws (surfaces as a toast) if it does, having
/// already opened the URL in the system browser as a courtesy.
export async function previewOpen(
  tab: TabId,
  url: string,
  rect: { x: number; y: number; width: number; height: number },
): Promise<void> {
  await invoke('preview_open', { tabId: tab, url, ...rect });
}

/// Navigate an already-open Preview tab to a new URL (the toolbar's URL
/// bar). Same policy check as `previewOpen`.
export async function previewNavigate(tab: TabId, url: string): Promise<void> {
  await invoke('preview_navigate', { tabId: tab, url });
}

/// Reload the Preview tab's current page (toolbar reload button + Phase F4
/// auto-reload).
export async function previewReload(tab: TabId): Promise<void> {
  await invoke('preview_reload', { tabId: tab });
}

/// Reposition/resize an open Preview tab's webview — called on every pane
/// layout change (the `ResizeObserver` on the measured body div, and the
/// device-preset letterbox rect from `computePreviewRect`).
export async function previewSetRect(
  tab: TabId,
  rect: { x: number; y: number; width: number; height: number },
): Promise<void> {
  await invoke('preview_set_rect', { tabId: tab, ...rect });
}

/// Hide (not destroy) a Preview tab's webview on tab-switch-away.
export async function previewHide(tab: TabId): Promise<void> {
  await invoke('preview_hide', { tabId: tab });
}

/// Show a previously-hidden Preview tab's webview on tab-switch-back.
export async function previewShow(tab: TabId): Promise<void> {
  await invoke('preview_show', { tabId: tab });
}

/// Destroy a Preview tab's webview on tab close (the pane body's
/// `onDestroy`). A missing/already-closed tab id is a no-op on the backend.
export async function previewClose(tab: TabId): Promise<void> {
  await invoke('preview_close', { tabId: tab });
}

/// Snapshot the Preview tab's current viewport to a PNG in the Phase-B
/// attach dir. Returns the saved path — the toolbar's Snapshot button then
/// pushes it onto `composeAttachments` and opens the compose overlay, same
/// as a pasted clipboard image.
export async function previewCapture(tab: TabId): Promise<string> {
  return invoke<string>('preview_capture', { tabId: tab });
}

/// Persist the toolbar's live `url`/`deviceWidth`/`autoReload` back onto the
/// tab's settings entry (so a restart reopens with the same state) and
/// remember `url` as the project's `preview_last_url`.
export async function previewUpdateConfig(
  tab: TabId,
  url: string,
  deviceWidth: number | null,
  autoReload: boolean,
): Promise<void> {
  await invoke('preview_update_config', {
    tabId: tab,
    url,
    deviceWidth,
    autoReload,
  });
}

/// Open `<portable-root>/logs/content/` in the OS file manager. Backend
/// creates the folder first if it doesn't exist.
export async function contentOpenFolder(): Promise<void> {
  await invoke('content_open_folder');
}

/// Delete every file inside `<portable-root>/logs/content/`. Returns the
/// count of removed files.
export async function contentClear(): Promise<number> {
  return invoke<number>('content_clear');
}

export interface PtyExitPayload {
  tab: TabId;
  exit: string;
}

export function onPtyExit(handler: (payload: PtyExitPayload) => void): Promise<UnlistenFn> {
  return listen<PtyExitPayload>('pty-exit', (event) => handler(event.payload));
}

export function decodeBase64(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

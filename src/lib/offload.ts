// V8-01 local task offload — frontend IPC wrappers + live status store.
// The backend supervisor owns the `llama-server` child; these drive its
// lifecycle from the Settings UI and mirror its `offload-state` events.

import { writable, type Writable } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { OpencodeLocalProvider, Settings } from './settings/types';

/// Mirror of Rust `OffloadState` (serde tag = "state").
export type OffloadState =
  | { state: 'disabled' }
  | { state: 'stopped' }
  | { state: 'starting' }
  | { state: 'ready'; n_ctx: number | null; slots: number; in_flight: number }
  | { state: 'error'; message: string };

export const offloadState: Writable<OffloadState> = writable({ state: 'disabled' });

let initialized = false;
let initInFlight: Promise<void> | null = null;

/// Fetch the current status and subscribe to live `offload-state` events.
/// Idempotent; safe to call on Settings mount.
export async function initOffloadStatus(): Promise<void> {
  if (initialized) return;
  // Dedupe concurrent callers (Settings mount + app mount) so we don't
  // subscribe twice while the first attempt is still in flight.
  if (initInFlight) return initInFlight;
  initInFlight = (async () => {
    // A failed status fetch is non-fatal — we still want the live listener.
    try {
      offloadState.set(await offloadStatus());
    } catch (e) {
      console.warn('offload_status failed', e);
    }
    await listen<OffloadState>('offload-state', (event) => {
      offloadState.set(event.payload);
    });
    // Mark initialized only after the subscription is established; if `listen`
    // throws, the flag stays false and the next call retries.
    initialized = true;
  })();
  try {
    await initInFlight;
  } catch (e) {
    console.warn('offload-state subscribe failed; will retry', e);
  } finally {
    initInFlight = null;
  }
}

export async function offloadStatus(): Promise<OffloadState> {
  return invoke('offload_status');
}

export async function offloadServerStart(): Promise<void> {
  await invoke('offload_server_start');
}

export async function offloadServerStop(): Promise<void> {
  await invoke('offload_server_stop');
}

export async function offloadServerRestart(): Promise<void> {
  await invoke('offload_server_restart');
}

/// V8-02: per-backend status row (mirror of Rust `BackendStatus`).
export interface BackendStatus {
  name: string;
  /// V39 Phase C: `harness` is a facade — a Remote-offload tab. It has no
  /// process and no URL, so its `state` is the delegation engine's own worker
  /// verdict and its `error` is that verdict's reason.
  kind: 'local' | 'lan' | 'cloud' | 'harness';
  tier: 'fast' | 'quality';
  enabled: boolean;
  cloud_blocked: boolean;
  state: 'disabled' | 'stopped' | 'starting' | 'ready' | 'unreachable' | 'blocked' | 'error';
  n_ctx: number | null;
  slots: number;
  in_flight: number;
  tool_scope: string;
  /// Failure reason when `state === 'error'` (e.g. a non-llama.cpp server on
  /// the configured port). `null` otherwise.
  error: string | null;
}

/// Per-backend status for the whole pool (Local process+health and Remote
/// health-probe). Drives the backends editor's status rows.
export async function offloadStatuses(): Promise<BackendStatus[]> {
  return invoke('offload_statuses');
}

/// Start a Local backend. `commandOverride` (from the Offload server dashboard's
/// "show command on start" popup) launches with that command instead of the
/// configured one for this start only — it is validated by the same backend
/// parse as the configured command and never persisted.
export async function offloadBackendStart(name: string, commandOverride?: string): Promise<void> {
  await invoke('offload_backend_start', { name, commandOverride: commandOverride ?? null });
}

export async function offloadBackendStop(name: string): Promise<void> {
  await invoke('offload_backend_stop', { name });
}

export async function offloadBackendRestart(name: string): Promise<void> {
  await invoke('offload_backend_restart', { name });
}

/// One-line summary of a per-backend status for the editor row.
export function describeBackendStatus(s: BackendStatus): string {
  const ctx = s.n_ctx ? `${s.n_ctx.toLocaleString()} ctx` : 'ctx unknown';
  switch (s.state) {
    case 'ready':
      return `Ready — ${ctx}, ${s.in_flight}/${s.slots} slots`;
    case 'starting':
      return 'Starting…';
    case 'stopped':
      return 'Stopped';
    case 'unreachable':
      return 'Unreachable';
    case 'blocked':
      return 'Needs cloud consent';
    case 'disabled':
      return 'Disabled';
    case 'error':
      return s.error ? `Error — ${s.error}` : 'Error';
    default:
      return s.state;
  }
}

/// Run a canned (or custom) offload task and return the synthesized answer.
export async function offloadTest(instructions: string): Promise<string> {
  return invoke('offload_test', { instructions });
}

/// V21: derive the OpenCode `local-llama` provider from a Local backend's
/// server command (the Settings "Add to OpenCode" button). Rejects with a
/// message naming the missing `--port`/model flag when the command is
/// incomplete; the caller persists the resolved snapshot via `settings_update`.
export async function offloadDeriveOpencodeProvider(
  serverCommand: string,
): Promise<OpencodeLocalProvider> {
  return invoke('offload_derive_opencode_provider', { serverCommand });
}

/// V8-03: per-MCP-server health row (mirror of Rust `McpServerHealth`).
export interface McpServerHealth {
  name: string;
  transport: 'stdio' | 'http';
  connected: boolean;
  healthy: boolean;
  tool_count: number;
  error: string | null;
  /// V37 contract C6: where the server sits in the health state machine
  /// (`Unknown -> Healthy <-> Unhealthy`). NOT a duplicate of `healthy`:
  /// `unknown` is "connected but never probed yet", which `healthy: true` alone
  /// cannot distinguish from "probed and fine".
  state: 'unknown' | 'healthy' | 'unhealthy';
  /// V37 contract C6: failed probes since the last success. Non-zero while
  /// `state` is still `healthy` is the flap guard mid-count — the one warning
  /// available before a server is declared down.
  consecutive_failures: number;
}

/// V8-03: aggregate offload-service status (mirror of Rust `ServiceStatus`).
/// `global_in_flight` is now honest — the long-lived app sees every offload
/// across all Claude tabs, so the warm-pool spill/fail-over works.
export interface ServiceStatus {
  global_in_flight: number;
  global_cap: number;
  /// Tasks waiting for a slot right now (app-wide queue depth).
  queue_depth: number;
  mcp_servers: McpServerHealth[];
}

/// Fetch the warm-pool service status (global in-flight + per-MCP-server
/// health). Returns `null` when the service isn't reachable (offload off or
/// app mid-launch) so the caller can render a neutral state.
export async function offloadServiceStatus(): Promise<ServiceStatus | null> {
  try {
    return await invoke<ServiceStatus>('offload_service_status');
  } catch (e) {
    console.warn('offload_service_status failed', e);
    return null;
  }
}

/// Reconcile the warm MCP host against the just-saved settings and return the
/// fresh status. Called by the Settings MCP editor right after persisting an
/// add/remove/enable/disable so the server connects or drops live — no
/// restart. Returns `null` if the service isn't reachable.
export async function offloadReloadMcp(): Promise<ServiceStatus | null> {
  try {
    return await invoke<ServiceStatus>('offload_reload_mcp');
  } catch (e) {
    console.warn('offload_reload_mcp failed', e);
    return null;
  }
}

/// V32 Phase C: how much of the injection-detection surface is actually live
/// (mirror of Rust `detection::DetectionStatus`). Read-only — everything here
/// is a fact about disk state, not a setting.
export interface DetectionStatus {
  rules: {
    /// Rule files that compiled and are live.
    files_loaded: number;
    /// Rule files found but rejected (compile error) — the number that matters:
    /// it means rules the user believes are active are not.
    files_failed: number;
    /// Individual rules across all loaded files.
    rules: number;
    /// Names of the rejected files.
    failed: string[];
    /// User rules from `rules.d/local/` that are live under a DIFFERENT
    /// identifier than their file spells, because a shipped rule already
    /// declares that identifier (#48, M-13). They compiled and they match —
    /// this is a notice, not a failure, which is why it is not part of
    /// `healthy`. The rename is applied on load; the file on disk is untouched.
    renamed: { file: string; from: string; to: string }[];
    /// Where cImp looked, so "0 files" is diagnosable.
    dir: string;
    /// `files_loaded > 0 && rules > 0` — this rule set can match something at
    /// all. **False is the disarmed layer**: every page it screens comes back
    /// clean because there is nothing to compare against, not because it is.
    armed: boolean;
    /// `armed && files_failed === 0` — the whole rule set on disk is live.
    ///
    /// Computed in Rust and read here, never restated (#48, N-3). This panel
    /// used to derive its own green dot as `files_failed === 0 &&
    /// files_loaded > 0`, omitting `rules` — which the updater's own health
    /// check requires — so a `.yar` file that parsed and defined no rules
    /// showed GREEN beside the literal text "1 file(s) loaded, 0 rule(s)"
    /// while `scan` returned empty.
    healthy: boolean;
  };
  classifier: {
    /// Weights found AND the ONNX session built.
    present: boolean;
    /// Where the weights are expected.
    dir: string;
    /// Why the classifier is not live, when it is not.
    error: string | null;
  };
  /// V32 Phase C3 (mirror of Rust `detection::updater::UpdaterStatus`): where
  /// the auto-updater stands for each component. Rides this same status so the
  /// Settings poller gets it in one round trip.
  updater: UpdaterStatus;
  /// #48: the state of the user's OWN `rules.d/local/` rules, when it is
  /// something they need to know — a file that does not compile, or a rule live
  /// under a renamed identifier (M-13). `null` in every healthy or irrelevant
  /// case. The same value the Advisor's `detection.local_rules_broken.v1` card
  /// is built from, so the Settings line and the card cannot disagree about
  /// whether the user's rules are live.
  ///
  /// The two lists are rendered separately: a renamed rule IS matching, and
  /// describing it in the broken file's words would be the same silent-lie
  /// shape the card exists to stop.
  local_rules_broken: {
    /// The folder the user can open to fix them.
    dir: string;
    /// Rejected file names, `local/`-prefixed.
    failed: string[];
    /// Rules live under a renamed identifier — empty in the ordinary case.
    renamed: { file: string; from: string; to: string }[];
    /// What IS live, so the line does not read as "detection is off".
    files_loaded: number;
    rules: number;
  } | null;
}

/// The one answer to "is the signature layer actually protecting this app?",
/// read off a detection status in exactly ONE place (#48, M-25).
///
/// Three states, because the rule set on disk has three — whole, partly live,
/// and matching nothing at all — and only the first is protection. The comment
/// on `healthy` above already said which field answers this question;
/// `latch.ts` branched on `armed` anyway and a rules directory with 3 of its 4
/// files failing to compile rendered as FULL protection. The type is the fix
/// the comment could not be: a caller that wants the safety question answered
/// takes this whole value, and `detectionContract.test.ts` fails the suite if a
/// second reader of `rules.armed` / `rules.healthy` appears.
export interface RulesHealth {
  /// `files_loaded > 0 && rules > 0` — the layer can match *something*.
  ///
  /// **Never the protected-or-not predicate on its own.** Its only job here is
  /// to separate *partly live* from *inert* once `healthy` has already said the
  /// set is not whole.
  armed: boolean;
  /// `armed && files_failed === 0` — the whole rule set on disk is live. THE
  /// predicate for "is this protection intact?", computed in Rust and never
  /// restated (#48, N-3).
  healthy: boolean;
  /// Rule files found and rejected, so a partial set can say how partial it is.
  /// Phrasing only: `files_failed === 0` is also true of an empty directory,
  /// which is why it cannot stand in for `healthy`.
  files_failed: number;
}

/// Read a detection status as [`RulesHealth`] — the sanctioned reader of
/// `rules.armed` / `rules.healthy`, alongside the Settings panel that renders
/// both raw.
export function rulesHealth(s: DetectionStatus): RulesHealth {
  return {
    armed: s.rules.armed,
    healthy: s.rules.healthy,
    files_failed: s.rules.files_failed,
  };
}

/// One updatable detection component's update state.
export interface DetectionComponentStatus {
  /// `rules` | `classifier` — the wire name every updater command takes.
  component: string;
  /// `off` | `check` | `auto`, matching the settings string exactly.
  mode: string;
  /// Empty until the first successful update: the shipped bundle carries no
  /// manifest version.
  installed_version: string;
  previous_version: string;
  /// True exactly when Revert has something to restore.
  can_revert: boolean;
  /// A newer version found but not applied — what Apply would take.
  available_version: string;
  /// The curator's note for it. REMOTE TEXT: rendered as text (Svelte escapes
  /// it), never as markup.
  available_notes: string;
  /// Epoch ms; 0 = never checked.
  last_check_ms: number;
  last_outcome: string;
  last_ok: boolean;
  /// The outcome's machine name: `up-to-date` | `available` | `applied` |
  /// `rejected` | `unavailable` | `reverted` | `revert-failed`. `unavailable`
  /// means the channel could not be REACHED — not that a bundle was refused —
  /// and must never be rendered as a failure (#46); `revert-failed` is a local
  /// revert that did not complete and says nothing about any bundle (#48).
  /// Empty on state written before this existed.
  last_outcome_kind: string;
  /// Consecutive checks that could not reach the channel. 0 once one does; a
  /// revert leaves it alone, having reached nothing either way.
  unreachable_streak: number;
  /// Non-empty when the last attempt was refused — a document arrived and a
  /// check said no. The old data is still live.
  last_failure: string;
  /// Rule files a rollback could not put back, so the live directory is SHORT
  /// of them (#48, M-11). The one field here that means "degraded right now":
  /// `last_ok` is about the last CHECK, and the rule counts describe what
  /// compiled, not what should have been present. Empty is healthy; the
  /// updater retries the restore on every check and every launch, and
  /// `detection.rules_incomplete.v1` cards it.
  unrestored_files: string[];
}

export interface UpdaterStatus {
  components: DetectionComponentStatus[];
  /// The manifest URL actually in use, so "nothing ever updates" is
  /// diagnosable without opening the settings file.
  manifest_url: string;
  interval_hours: number;
  rules_dir: string;
  state_dir: string;
  /// #48 (M-21): whether the updater may do anything at all — Rust's
  /// `updater::updates_enabled`, resolved at the APP scope with the master
  /// switch folded in.
  ///
  /// **This is the predicate the two IPC commands enforce**, published so a
  /// surface that greys a control reads it rather than assembling a second
  /// opinion out of the resolved-scope matrix. Two predicates for one question
  /// is how a greyed button and a served command come to disagree.
  updates_enabled: boolean;
  /// #48 (M-21): injection detection is armed for the OFFLOAD WORKER while this
  /// updater is inert — the one state in which "injection detection is off" is a
  /// false statement about this install. The worker keeps screening with the rule
  /// bundle already on disk, so the bundle still matters even though nothing will
  /// refresh it.
  ///
  /// Only ever true when `updates_enabled` is false, so the pair reads as one
  /// three-valued answer: on / off / off here but on in the worker. Never a
  /// verdict — no surface may use it to admit an update.
  worker_only_detection: boolean;
}

/// Read the detection status, **propagating the failure**. `reload = true`
/// recompiles the YARA rules from disk first — what the "Reload rules" button
/// calls after the user edits a file in `detection/rules.d/local/`.
///
/// The non-swallowing form exists because of #48, H-10: the swallowing one below
/// hands its caller a `null` that means "could not read", and a caller that
/// treats "could not read" as "nothing to report" renders the signature layer as
/// ARMED. Any surface whose default rendering is *reassuring* must take this
/// form and decide what a failure looks like — see `latch.ts`'s
/// [`recordSignatureRead`].
export async function fetchDetectionStatus(reload = false): Promise<DetectionStatus> {
  return invoke<DetectionStatus>('detection_status', { reload });
}

/// The same read with the failure swallowed to `null`, for callers whose
/// *rendering of `null` is itself the honest answer* — today only the Settings
/// detection panel, which renders "cImp could not read the detection layer's
/// status" in that case.
///
/// **Do not feed this `null` to a surface that would read it as "all clear"**
/// (#48, H-10). It means "we could not tell", which is a third state, not the
/// absence of news.
export async function detectionStatus(reload = false): Promise<DetectionStatus | null> {
  try {
    return await fetchDetectionStatus(reload);
  } catch (e) {
    console.warn('detection_status failed', e);
    return null;
  }
}

/// V32 Phase C3: run an update check now. `component` omitted checks both;
/// `apply = true` overrides a `check-only` mode for this one run (never `off`).
/// Awaits the whole run — download, validation and swap included — and returns
/// the refreshed status, so the caller re-renders the outcome rather than
/// polling for it.
export async function detectionCheckNow(
  component?: string,
  apply = false,
): Promise<DetectionStatus | null> {
  try {
    return await invoke<DetectionStatus>('detection_check_now', {
      component: component ?? null,
      apply,
    });
  } catch (e) {
    console.warn('detection_check_now failed', e);
    return null;
  }
}

/// V32 Phase C3: restore a component's retained previous version.
export async function detectionRevert(component: string): Promise<DetectionStatus | null> {
  try {
    return await invoke<DetectionStatus>('detection_revert', { component });
  } catch (e) {
    console.warn('detection_revert failed', e);
    return null;
  }
}

/// V32 Phase C3: open `detection/rules.d/` in the host file manager.
export async function detectionOpenRulesFolder(): Promise<void> {
  try {
    await invoke('detection_open_rules_folder');
  } catch (e) {
    console.warn('detection_open_rules_folder failed', e);
  }
}

/// V21 F7: merge the curated read-only command preset (`git` + `cargo`
/// metadata/tree, with the `cargo` policy that pins it to those verbs) into the
/// live offload settings, and return the updated Settings so the caller can
/// refresh its snapshot. Idempotent — re-invoking adds nothing and never
/// clobbers a user-authored `cargo` policy. Returns `null` if the command
/// fails (e.g. the app isn't reachable).
export async function offloadEnableReadonlyCommands(): Promise<Settings | null> {
  try {
    return await invoke<Settings>('offload_enable_readonly_commands');
  } catch (e) {
    console.warn('offload_enable_readonly_commands failed', e);
    return null;
  }
}

/// V8-03: a captured `llama-server` output line (mirror of Rust `ServerLogLine`).
export interface ServerLogLine {
  backend: string;
  line: string;
}

/// Buffered server output (model-load progress + logs) for a backend, or the
/// primary backend when `name` is omitted. The read-only log panel's initial
/// fill; subscribe to `onOffloadServerOutput` for live lines.
export async function offloadServerLog(name?: string): Promise<string[]> {
  return invoke('offload_server_log', { name: name ?? null });
}

/// Subscribe to live `llama-server` output lines. Returns an unlisten fn.
export function onOffloadServerOutput(
  cb: (line: ServerLogLine) => void,
): Promise<UnlistenFn> {
  return listen<ServerLogLine>('offload-server-output', (e) => cb(e.payload));
}

/// V8-03: Offload server dashboard snapshot (mirror of Rust `ServerMetrics`).
export interface SlotMetric {
  id: number;
  processing: boolean;
  /// Prompt (input) tokens. Total context in use = n_prompt + n_decoded.
  n_prompt: number;
  n_decoded: number;
  n_ctx: number;
  tps: number | null;
}
export interface RequestRecord {
  slot: number;
  start_ms: number;
  end_ms: number;
  duration_s: number;
  /// Prompt (input) tokens. Total tokens = prompt_tokens + tokens.
  prompt_tokens: number;
  /// Generated (output) tokens.
  tokens: number;
  avg_tps: number;
}
/// One LLM call within an offload run (mirror of Rust `CallRecord`).
export interface CallRecord {
  step: number;
  /// 'planning' | 'ingestion' | 'final'.
  kind: string;
  thinking: boolean;
  prompt_tokens: number;
  output_tokens: number;
  duration_ms: number;
  tps: number;
  /// 'tool_calls(N)' | 'answer' | 'empty' | 'leaked' | 'error'.
  result: string;
}
/// One offload run grouping its LLM calls (mirror of Rust `RunRecord`).
export interface RunRecord {
  id: number;
  instructions: string;
  /// Initial thinking mode: 'on' | 'off' | 'auto'.
  thinking: string;
  started_ms: number;
  /// 0 while still running.
  ended_ms: number;
  /// 'running' | 'success' | 'recovered' | 'failed'.
  outcome: string;
  /// V21 F5: 'fast' when this run was escalated from the fast tier to the
  /// quality backend after a partial result. Absent for normal runs.
  escalated_from?: string | null;
  calls: CallRecord[];
}
export interface ServerMetrics {
  running: boolean;
  total_slots: number;
  n_ctx_per_slot: number | null;
  busy_slots: number;
  slots: SlotMetric[];
  kv_cache_pct: number | null;
  predicted_tps: number | null;
  prompt_tps: number | null;
  requests_deferred: number | null;
  aggregate_tps: number;
  global_in_flight: number;
  global_cap: number;
  /// App-wide tasks waiting for a slot right now (stamped by the poller).
  queue_depth: number;
  metrics_available: boolean;
  history: RequestRecord[];
  /// Offload runs (one per offload_task), newest first, each grouping calls.
  runs: RunRecord[];
}

/// One backend's dashboard card (mirror of Rust `BackendDashboard`). `kind`
/// drives the Local-vs-Remote grouping; `state` is the coarse lifecycle that
/// decides whether the live dashboard or a status line renders.
export interface BackendDashboard {
  name: string;
  kind: 'local' | 'lan' | 'cloud' | 'harness';
  state: 'ready' | 'stopped' | 'starting' | 'unreachable' | 'blocked' | 'disabled';
  metrics: ServerMetrics;
}

/// Latest dashboard snapshot — one row per enabled backend. Empty before the
/// first poll (or when offload is disabled).
export async function offloadServerMetrics(): Promise<BackendDashboard[]> {
  try {
    return await invoke<BackendDashboard[]>('offload_server_metrics');
  } catch (e) {
    console.warn('offload_server_metrics failed', e);
    return [];
  }
}

/// Subscribe to live dashboard snapshots (one row per backend). Returns an
/// unlisten fn.
export function onOffloadServerMetrics(
  cb: (rows: BackendDashboard[]) => void,
): Promise<UnlistenFn> {
  return listen<BackendDashboard[]>('offload-server-metrics', (e) => cb(e.payload));
}

/// One-line summary of an MCP-server health row for the Settings list.
///
/// **The single formatter** (V37): the health chip in `McpManagementEditor` is
/// its only render site, so the state machine's vocabulary is defined once. A
/// second formatter is how "unhealthy" and "one probe short of unhealthy" end up
/// reading the same in one place and differently in another.
export function describeMcpServerHealth(s: McpServerHealth): string {
  const probes = (n: number) => `${n} consecutive failed probe${n === 1 ? '' : 's'}`;
  // Checked before `healthy` because the two are set together by the backend
  // and this is the more specific claim — it names the evidence, not just the
  // verdict.
  if (s.state === 'unhealthy') {
    const why = s.error ? ` — ${s.error}` : '';
    return `Unhealthy${why} (${probes(s.consecutive_failures)}, ${s.transport})`;
  }
  if (s.healthy) {
    const base = `Healthy — ${s.tool_count} tool${s.tool_count === 1 ? '' : 's'} (${s.transport})`;
    // A failure short of the flap guard changes no state and withdraws no
    // tools, but it is the only warning there is before one that does. Silence
    // here would make a server that is visibly wobbling look untouched.
    return s.consecutive_failures > 0
      ? `${base} · ${probes(s.consecutive_failures)} since the last success`
      : base;
  }
  if (s.connected) {
    return `Connected, no tools (${s.transport})`;
  }
  return s.error ? `Down — ${s.error}` : 'Down';
}

/// One-line human-readable summary of a status for the Settings readout.
export function describeOffloadState(s: OffloadState): string {
  switch (s.state) {
    case 'disabled':
      return 'Disabled';
    case 'stopped':
      return 'Stopped (starts on first offload, or click Start)';
    case 'starting':
      return 'Starting — loading model…';
    case 'ready': {
      const ctx = s.n_ctx ? `${s.n_ctx.toLocaleString()} ctx` : 'ctx unknown';
      return `Ready — ${ctx}, ${s.in_flight}/${s.slots} slots in use`;
    }
    case 'error':
      return `Error — ${s.message}`;
  }
}

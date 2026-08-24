use std::sync::Arc;

use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::error::{AppError, AppResult};
use crate::ipc::windows::{open_or_focus_settings, SETTINGS_LABEL};
use crate::ipc::AppState;
use crate::pty::PtyHost;
use crate::service::pty::PtyService;
use crate::service::audio::AudioService;
use crate::service::checks::{ApplySummary, ChecksService, ChecksSuggestion};
use crate::service::delegation::DelegationControls;
use crate::service::settings::SettingsService;
use crate::service::sink::{OutputSink, TauriEventSink};
use crate::service::window::{SettingsDeepLink, SettingsWindow};
use crate::service::graph::{
    ArchResult, CodeIntelService, DeadExportRow, ImpactResult, PathResult, VizFileStatusRow,
    VizGraphResult,
};
use crate::service::harness::{HarnessService, HarnessStatus, HarnessUsage};
use crate::service::offload::{OffloadServerUseCases, OffloadServiceUseCases};
use crate::service::usage::{AdvisorRules, AdvisorSnapshot, UsageService};
use crate::service::workbench::WorkbenchUseCases;
use crate::settings::{AiToolTabConfig, Settings};
use crate::state::TabId;

/// V1.4-04 D: `pty_start` returns the persisted-scrollback bytes from the
/// previous session (if any) — see [`PtyService::start`] for the whole
/// contract. This is the wire boundary only: it names the two things the
/// service cannot get for itself, the app host ([`PtyHost::from_app`]) and the
/// frontend's `Channel`, and hands everything else off.
#[tauri::command]
pub async fn pty_start(
    app: AppHandle,
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<Option<Vec<u8>>> {
    pty_service(&state)
        .start(
            PtyHost::from_app(&app),
            tab,
            Arc::new(channel) as Arc<dyn OutputSink>,
            rows,
            cols,
        )
        .await
}

/// Build the PTY service over this app's handles. One place, so the three PTY
/// commands cannot drift in what they hand it.
fn pty_service<'a>(state: &'a AppState) -> PtyService<'a> {
    PtyService::new(
        &state.tabs,
        &state.settings,
        &state.tab_activity,
        &state.tts_segments,
        &state.launch.cwd,
        &state.launch.extra_args,
        &state.read_only,
        &state.input_lengths,
        &state.state_signals,
    )
}

/// Tear a tab's subprocess down and bring a fresh one up on a new channel.
/// See [`PtyService::restart`].
#[tauri::command]
pub async fn pty_restart(
    app: AppHandle,
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    pty_service(&state)
        .restart(
            PtyHost::from_app(&app),
            tab,
            Arc::new(channel) as Arc<dyn OutputSink>,
            rows,
            cols,
        )
        .await
}

/// V1.4-03: re-point a still-running PTY's bytes at a fresh JS-side
/// `Channel<String>` without restarting the shell. The frontend invokes
/// this when the xterm.js Terminal is destroyed and recreated for a
/// renderer-category flip (background image toggled on or off). The
/// shell session, env, cwd, and any in-flight processes survive; only
/// the IPC channel is replaced.
#[tauri::command]
pub async fn pty_rebind_channel(
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
) -> AppResult<()> {
    pty_service(&state)
        .rebind(tab, Arc::new(channel) as Arc<dyn OutputSink>)
        .await
}

/// V1.4-04 D.3: snapshot a tab's PTY scrollback as raw bytes. Exposed
/// for diagnostics and external use; the launch-replay path uses an
/// internal API (`pty_start` returning `Option<Vec<u8>>`) for
/// efficiency. Returns `NotStarted` if the tab has no live PTY.
#[tauri::command]
pub async fn pty_get_scrollback(state: State<'_, AppState>, tab: TabId) -> AppResult<Vec<u8>> {
    pty_service(&state).scrollback(tab).await
}

// V42 Phase A2: `write_through_pipeline` used to sit here — a one-line adapter
// giving the delegation engine a door into the input pipeline over `&AppState`,
// because the engine had no handles of its own and resolved everything through
// `AppHandle::state::<AppState>()`. A2 gave it a
// [`CoreHost`](crate::service::host::CoreHost), so the engine calls
// `host.pty().write_through(..)` directly and the adapter — and the `Submit`
// re-export that existed to be named beside it — are gone. The pipeline, its
// two call sites and the source-scanned property that they disagree about
// `Submit` are unchanged; only the spelling the scanners look for moved.

/// Deliver one chunk of keyboard input to a tab's PTY. See
/// [`PtyService::write`] for the read-only enforcement point and the terminal-
/// protocol exemption, and [`PtyService::write_through`] for the pipeline every
/// byte passes through.
#[tauri::command]
pub async fn pty_write(state: State<'_, AppState>, tab: TabId, input: String) -> AppResult<()> {
    pty_service(&state).write(tab, input).await
}

/// Build the per-tab delegation controls over this app's handles. One place,
/// so the three popover commands cannot drift in what they hand them.
fn delegation_controls(state: &AppState) -> DelegationControls<'_> {
    DelegationControls::new(&state.settings, &state.tabs, &state.read_only)
}

/// V39 Phase A: set or clear a tab's **user** read-only lock — the Access radio
/// in the tab's communication popover. See
/// [`DelegationControls::set_read_only`] for why the runtime lock is taken
/// before the persisted flag is written.
#[tauri::command]
pub async fn tab_set_read_only(state: State<'_, AppState>, tab: TabId, on: bool) -> AppResult<()> {
    delegation_controls(&state).set_read_only(&tab, on)
}

/// V39 Phase B: what [`tab_set_delegation_role`] did, for the UI's toast.
/// Defined in [`service::delegation`](crate::service::delegation) and named
/// here because `src/lib/delegation.ts`'s mirror points at this path.
pub use crate::service::delegation::RoleChange;

/// V39 Phase B (locked decision 8): set a tab's delegation role, enforcing
/// **at most one Manual tab per harness**.
///
/// The move rule, and why it is a move rather than a refusal: the user is
/// choosing which tab `delegate_task_<harness>` drives, and there is exactly
/// one answer per harness. A refusal ("tab A already holds it") would make the
/// user go and clear A first for no reason anyone benefits from — so this is a
/// radio button whose group spans tabs. The previous holder drops to `None`,
/// both writes land in ONE settings mutation (so no broadcast can observe two
/// Manual tabs of one harness), an Events row records the move, and the
/// displaced id comes back for the toast.
///
/// Refusals, each naming its condition: a reserved dashboard (no PTY, no
/// harness), a tab that is not a configured AI tab, and a harness with no input
/// profile (it could never be driven, so a role on it would be a control that
/// does nothing).
///
/// **Not spawn-baked** (decision 15). The persisted write broadcasts
/// `settings-changed`, which is also what asks the offload service for a
/// `tools/list_changed` pulse — `graph::native_surface_sig` now hashes the
/// Manual set, so the gate sees the move and every live session re-lists on its
/// next turn without a restart.
#[tauri::command]
pub async fn tab_set_delegation_role(
    state: State<'_, AppState>,
    tab: TabId,
    role: crate::settings::DelegationRole,
) -> AppResult<RoleChange> {
    delegation_controls(&state).set_role(&tab, role).await
}

/// **Write one tab's facade-backend knobs, and nothing else** (V39 review
/// M-10).
///
/// The popover used to save these through the ordinary whole-document
/// `applySettings`: read the store, patch three fields, send the entire
/// `Settings`. That is the `40d2b32` lost-update shape — a document written
/// from a snapshot taken before some other write landed silently reverts it —
/// and the write most likely to be in flight beside it is the ROLE radio one
/// line above, which goes through `tab_set_delegation_role` precisely because
/// only the backend can enforce its cross-tab rule. Typing a backend name
/// could put the role back.
///
/// So: one command, three fields, `settings.mutate` (which composes with a
/// concurrent mutation instead of overwriting the document).
///
/// **The role is deliberately not touched**, and neither is anything else on
/// the tab: a user who sets a name, switches the role away and switches it
/// back finds the knobs where they left them.
#[tauri::command]
pub async fn tab_set_delegation_backend(
    state: State<'_, AppState>,
    tab: TabId,
    backend: crate::settings::DelegationBackend,
) -> AppResult<()> {
    delegation_controls(&state).set_backend(&tab, backend)
}

/// V39 Phase B (locked decision 6): **take over** a driven tab.
///
/// Stops the driver waiting; the worker keeps running, visibly. Sends the
/// worker NOTHING — no Escape, no interrupt.
///
/// **Sets a flag, and that is all it does.** The engine's own path releases the
/// read-only lock and mints the single `takeover` Events row on its way out —
/// two owners of a teardown is how one of them ends up running twice, which is
/// exactly what happened in this phase's first cut: this command minted a
/// `takeover` row and the engine minted a `cancelled` one, two rows for one
/// event. One event, one row, minted where the flight ends and the timings are
/// known.
///
/// Returns whether a delegation was actually in flight, so the UI can tell "I
/// cancelled it" from "it had already finished".
///
/// **Left as a direct call** (V42 Phase A): the body is one call on
/// [`crate::delegation`]'s process-global registry, which a test reaches
/// already — no `State`, no `AppHandle`, nothing to shape. Same for
/// [`delegation_status`] and [`delegation_statuses`] below. The registry's own
/// ambient-global shape is V42 #114's question, not a wrap's.
#[tauri::command]
pub async fn delegation_take_over(tab: TabId) -> AppResult<bool> {
    Ok(crate::delegation::take_over(&tab).is_some())
}

/// V39 Phase B: what is driving `tab` right now, if anything — the glyph's
/// *driven* state and the worker-tab banner.
///
/// A pull to pair with the `delegation-changed` push: the event carries every
/// edge, and this is what a view that mounts mid-flight asks so it paints the
/// right thing before the next edge arrives.
///
/// **Left as a direct call**, for [`delegation_take_over`]'s reason.
#[tauri::command]
pub async fn delegation_status(tab: TabId) -> AppResult<Option<crate::delegation::InFlightView>> {
    Ok(crate::delegation::status(&tab))
}

/// V39 Phase B: every in-flight delegation, keyed by worker tab id — the
/// status-bar chip's count and the initial paint of every tab's glyph, in one
/// call rather than one per tab.
///
/// **Left as a direct call**, for [`delegation_take_over`]'s reason.
#[tauri::command]
pub async fn delegation_statuses() -> AppResult<Vec<(String, crate::delegation::InFlightView)>> {
    Ok(crate::delegation::statuses())
}

/// Build the speech use cases over this app's handles. One place, so the five
/// TTS commands cannot drift in what they hand them.
fn audio_service(state: &AppState) -> AudioService<'_> {
    AudioService::new(
        &state.tabs,
        &state.tts_segments,
        &state.speak_session,
        &state.ai_tts_suppressed,
        &state.audio,
    )
}

/// Debug: synthesize and play `text` directly through the TTS worker, skipping
/// the processor. Routed as if it came from the active tab so the worker's
/// filter doesn't drop it. See [`AudioService::test`].
#[tauri::command]
pub async fn tts_test(state: State<'_, AppState>, text: String) -> AppResult<()> {
    audio_service(&state).test(text).await
}

/// Read arbitrary text aloud through the TTS worker, skipping the
/// processor. Backs the Ctrl+right-click "speak selection" gesture
/// (`behavior.speak_selection_on_right_click`). Routed as if it came
/// from the active tab so the worker's background-tab filter doesn't
/// drop it. Whitespace-only text is ignored — the frontend guards too,
/// but a backend skip keeps an empty synthesis off the worker.
#[tauri::command]
pub async fn tts_speak(state: State<'_, AppState>, text: String) -> AppResult<()> {
    audio_service(&state).speak(text).await
}

/// Read a terminal selection aloud as a read-along: `chunks` are the
/// sentence segments (pre-split on the frontend so the spoken text exactly
/// matches the highlighted text), synthesized and played in order. `session`
/// is a frontend-assigned monotonic id stored in the shared cell so the
/// worker can be told to abandon the read — `tts_stop` (Esc) zeroes the
/// cell, and a newer call overwrites it. The audio thread emits
/// `tts-selection-progress` events as it advances through the chunks so the
/// frontend can recede the highlight. Backs `behavior.speak_selection_on_right_click`.
#[tauri::command]
pub async fn tts_speak_selection(
    state: State<'_, AppState>,
    session: u64,
    chunks: Vec<String>,
) -> AppResult<()> {
    audio_service(&state)
        .speak_selection(session, chunks)
        .await
}

/// Stop all TTS playback immediately and cancel any in-flight selection read.
/// Backs the Esc gesture: clears the audio sink (so queued chunks never play)
/// and zeroes the shared session cell (so the worker abandons the remaining
/// chunks it hasn't enqueued yet). The frontend clears its highlight on the
/// same Esc.
#[tauri::command]
pub async fn tts_stop(state: State<'_, AppState>) -> AppResult<()> {
    audio_service(&state).stop();
    Ok(())
}

/// Pause or resume TTS playback without discarding queued audio. Backs the
/// bottom-bar selection-TTS pause/resume transport. The in-flight read's
/// session is left untouched (so resume continues exactly where it paused);
/// only the audio sink is paused.
#[tauri::command]
pub async fn tts_set_paused(state: State<'_, AppState>, paused: bool) -> AppResult<()> {
    audio_service(&state).set_paused(paused);
    Ok(())
}

// --- Speech-to-text (V6-01) -------------------------------------------------
//
// The handle just posts commands to the capture thread; recording/transcribe
// state transitions and the resulting transcript arrive on the frontend via
// the `stt-state` / `stt-transcription` events, not these return values.

/// Open the input device and begin capturing. No-op if already recording.
///
/// **Left as a direct call** (V42 Phase A): the body is one post to the capture
/// thread on the handle Tauri injected. There is no argument to shape, no
/// ordering to get right and no return value — everything the user sees comes
/// back as an `stt-state` / `stt-transcription` event, so a service in front of
/// this would have nothing to assert. Same for the two commands below and for
/// the two `stt_list_*` readers.
#[tauri::command]
pub async fn stt_start_recording(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.start();
    Ok(())
}

/// Stop capturing and hand the recording to the transcription worker. The
/// transcript arrives later via the `stt-transcription` event.
///
/// **Left as a direct call**, for [`stt_start_recording`]'s reason.
#[tauri::command]
pub async fn stt_stop_recording(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.stop();
    Ok(())
}

/// Stop capturing and discard the buffer (no transcription).
///
/// **Left as a direct call**, for [`stt_start_recording`]'s reason.
#[tauri::command]
pub async fn stt_cancel(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.cancel();
    Ok(())
}

/// List the `ggml-*.bin` Whisper models present under `models/` for the
/// settings dropdown.
///
/// **Left as a direct call**, for [`stt_start_recording`]'s reason: the body is
/// one call on a free function in [`crate::stt`], which a test can already
/// reach.
#[tauri::command]
pub async fn stt_list_models() -> AppResult<Vec<String>> {
    crate::stt::list_models()
}

/// List cpal input device names for the settings device picker. The frontend
/// prepends a "System default" entry (which maps to an empty `input_device`).
///
/// **Left as a direct call**, for [`stt_list_models`]'s reason.
#[tauri::command]
pub async fn stt_list_input_devices() -> AppResult<Vec<String>> {
    crate::stt::list_input_devices()
}

/// **One harness's usage reading** for the bottom-bar tracker (V40 Phase D,
/// locked decision 19). Local read, never the network. See
/// [`service::harness::usage`](crate::service::harness::usage) for the three
/// distinguishable source states, for why the declared turn shape sits BESIDE
/// them rather than inside them, and for why an unregistered harness id is an
/// error rather than an empty reading.
#[tauri::command]
pub async fn harness_usage(harness: String) -> AppResult<HarnessUsage> {
    crate::service::harness::usage(&harness)
}

/// Sample the system-monitor stats (CPU / memory / GPU / network) for the
/// bottom-bar panel. Polled by the frontend on `system_stats.poll_interval_secs`
/// (default 1s); the frontend keeps its own history for the sparklines.
#[tauri::command]
pub async fn get_system_stats(
    state: State<'_, AppState>,
) -> AppResult<crate::sysmon::SystemStatsSnapshot> {
    crate::service::view::system_stats(state.sysmon.clone()).await
}

#[tauri::command]
pub async fn pty_resize(
    state: State<'_, AppState>,
    tab: TabId,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    pty_service(&state).resize(tab, rows, cols).await
}

/// The compose overlay's non-empty edge. See
/// [`TabService::compose_content_changed`](crate::service::tabs::TabService::compose_content_changed)
/// for why the target tab is resolved on this side.
#[tauri::command]
pub async fn compose_content_changed(state: State<'_, AppState>, non_empty: bool) -> AppResult<()> {
    crate::ipc::tab_lifecycle::tab_service(&state)
        .compose_content_changed(non_empty)
        .await;
    Ok(())
}

/// V14 Phase A: the compose overlay's `/` picker data source — the global
/// prompt-template library resolved against `root`'s project-scope additions.
/// `root` defaults to the launch directory, mirroring `graph_rebuild`. See
/// [`service::settings::resolved_prompt_templates`](crate::service::settings::resolved_prompt_templates).
#[tauri::command]
pub async fn compose_templates(
    root: Option<String>,
) -> AppResult<Vec<crate::settings::ResolvedTemplate>> {
    let root = resolve_graph_root(root)?;
    Ok(crate::service::settings::resolved_prompt_templates(&root))
}

/// V14 Phase A: the Settings window's Compose section reads the raw global
/// list (unshadowed). See
/// [`service::settings::global_prompt_templates`](crate::service::settings::global_prompt_templates).
#[tauri::command]
pub async fn compose_templates_global_get() -> AppResult<Vec<crate::settings::PromptTemplate>> {
    Ok(crate::service::settings::global_prompt_templates())
}

/// V14 Phase A: the Settings window's Compose section save. See
/// [`service::settings::set_global_prompt_templates`](crate::service::settings::set_global_prompt_templates).
#[tauri::command]
pub async fn compose_templates_global_set(
    templates: Vec<crate::settings::PromptTemplate>,
) -> AppResult<()> {
    crate::service::settings::set_global_prompt_templates(templates)
}

/// LLM price table for the session-cost popup and its Settings editor. See
/// [`service::settings::llm_pricing`](crate::service::settings::llm_pricing).
#[tauri::command]
pub async fn llm_pricing_get() -> AppResult<Vec<crate::settings::LlmPricingModel>> {
    Ok(crate::service::settings::llm_pricing())
}

/// Save the LLM price table straight to the physical global `settings.json`.
/// See
/// [`service::settings::set_llm_pricing`](crate::service::settings::set_llm_pricing)
/// — this is the wire boundary only: it names the sink the out-of-band write's
/// own `llm-pricing-changed` announcement goes to.
#[tauri::command]
pub async fn llm_pricing_set(
    app: AppHandle,
    pricing: Vec<crate::settings::LlmPricingModel>,
) -> AppResult<()> {
    crate::service::settings::set_llm_pricing(pricing, &TauriEventSink::new(app))
}

/// V14 Phase A: read-only project-scope listing for the Settings window's
/// Compose section. `root` defaults to the launch directory. See
/// [`service::settings::project_prompt_templates`](crate::service::settings::project_prompt_templates).
#[tauri::command]
pub async fn compose_templates_project_get(
    root: Option<String>,
) -> AppResult<Vec<crate::settings::PromptTemplate>> {
    let root = resolve_graph_root(root)?;
    Ok(crate::service::settings::project_prompt_templates(&root))
}

/// V14 Phase B: writes a pasted clipboard image (already re-encoded to PNG
/// bytes on the frontend — see `lib/compose/attachments.ts`'s
/// `readClipboardImagePng`, which reads via the Tauri clipboard plugin's
/// image API rather than the WebView2-denied `navigator.clipboard`) to this
/// app run's session-scoped attach dir and returns the absolute saved path.
/// The frontend renders that path as a chip and, on submit, appends it to
/// the message text (`compose/attachments.ts`'s `appendAttachments`).
/// Dropped image *files* (`tauri://drag-drop`) skip this command entirely —
/// they're referenced in place, never copied here.
///
/// **Left as a direct call** (V42 Phase A): one call on
/// [`crate::attach::save_png`], which owns the session-scoped directory rule
/// and is already reachable from a test, plus the lossy path-to-string the wire
/// needs.
#[tauri::command]
pub async fn compose_attach_image(state: State<'_, AppState>, bytes: Vec<u8>) -> AppResult<String> {
    let session = state.launch.launch_id.clone();
    let path = crate::attach::save_png(&session, &bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

/// The user dismissed a tab's error badge. See
/// [`TabService::acknowledge_error`](crate::service::tabs::TabService::acknowledge_error).
#[tauri::command]
pub async fn acknowledge_error(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    crate::ipc::tab_lifecycle::tab_service(&state).acknowledge_error(tab);
    Ok(())
}

/// Activate a tab. Frontend calls this on click and on Ctrl+1/Ctrl+2; the
/// state manager broadcasts an `ActiveTabChanged` event so all subscribers
/// reconcile from a single source of truth. Does NOT persist the active
/// tab to settings — use `set_active_tab` for that.
#[tauri::command]
pub async fn tab_activate(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    crate::ipc::tab_lifecycle::tab_service(&state)
        .activate(tab)
        .await
}

/// Activate a tab AND persist its id as `session.active_tab_id`. Used by
/// the frontend's tab-switch handler (click, Ctrl+1..9) so the user's
/// last-active tab is restored on next launch. The settings write is
/// debounced so a fast Ctrl+1/Ctrl+2 burst doesn't hammer the disk.
#[tauri::command]
pub async fn set_active_tab(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    crate::ipc::tab_lifecycle::tab_service(&state)
        .set_active(tab)
        .await
}

/// Snapshot the live tab list. Frontend calls this once on App mount to
/// seed its tabs store; subsequent runtime mutations arrive via the
/// `tab-created`/`tab-closed`/`tab-renamed` events broadcast through the
/// `avatar-state` channel. Avoids the race where setup-time TabCreated
/// emissions could fire before the webview's listener attaches.
#[tauri::command]
pub async fn list_tabs(state: State<'_, AppState>) -> AppResult<Vec<crate::tabs::TabMetaWire>> {
    Ok(crate::ipc::tab_lifecycle::tab_service(&state).list().await)
}

/// The live in-memory settings snapshot. See [`SettingsService::get`].
#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> AppResult<Settings> {
    Ok(settings_service(&state).get())
}

/// Build the settings service over this app's handles. One place, so the
/// settings commands cannot drift in what they hand it.
fn settings_service(state: &AppState) -> SettingsService<'_> {
    SettingsService::new(
        &state.settings,
        &state.tabs,
        &state.state_signals,
        &state.lifecycle_serializer,
        &state.stt,
    )
}

/// V21 F7: merge the curated read-only command preset (`git` + `cargo`
/// metadata/tree) into the live offload settings and return the updated
/// snapshot. See [`SettingsService::enable_readonly_commands`].
#[tauri::command]
pub async fn offload_enable_readonly_commands(state: State<'_, AppState>) -> AppResult<Settings> {
    Ok(settings_service(&state).enable_readonly_commands())
}

/// Per-AI-tab default config, for the Settings window's "Reset to default"
/// buttons. See
/// [`service::settings::ai_tool_tab_defaults`](crate::service::settings::ai_tool_tab_defaults).
#[tauri::command]
pub async fn ai_tool_tab_defaults(tab: TabId) -> AppResult<AiToolTabConfig> {
    crate::service::settings::ai_tool_tab_defaults(&tab)
}

/// Apply the Settings window's whole-struct save. See
/// [`SettingsService::update`] for the ordering contract and the five edges the
/// save computes across one atomic write. This is the wire boundary only: it
/// names the two collaborators the service cannot get for itself — the warm
/// code-graph index and the event sink.
#[tauri::command]
pub async fn settings_update(
    app: AppHandle,
    state: State<'_, AppState>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    settings: Settings,
) -> AppResult<()> {
    settings_service(&state)
        .update(settings, graph.inner(), &TauriEventSink::new(app))
        .await
}

// ── V8-01 local task offload ────────────────────────────────────────────
// The supervisor is managed as its own state (it needs the AppHandle for
// `offload-state` events, available only in the setup hook). These thin
// commands drive its lifecycle from the Settings UI.

/// Build the offload server/pool use cases over this app's handle. One place,
/// so no command can drift in what it hands them.
fn offload_server_use_cases(
    supervisor: &std::sync::Arc<crate::offload::OffloadSupervisor>,
) -> OffloadServerUseCases<'_> {
    OffloadServerUseCases::new(supervisor)
}

/// Build the warm-offload-service use cases over this app's handle. Separate
/// from [`offload_server_use_cases`] because the two are separate managed
/// states: the supervisor owns the processes, the service owns the MCP host and
/// the dashboard, and no command needs both.
fn offload_service_use_cases(
    service: &std::sync::Arc<crate::offload::OffloadService>,
) -> OffloadServiceUseCases<'_> {
    OffloadServiceUseCases::new(service)
}

/// Current offload server status (state + discovered `n_ctx`/slots/in-flight)
/// for the **primary** local backend (legacy single-status readout).
#[tauri::command]
pub async fn offload_status(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<crate::offload::OffloadState> {
    Ok(offload_server_use_cases(&supervisor).primary_state().await)
}

/// V8-02: per-backend status for every enabled backend in the pool (Local
/// process+health and Remote health-probe). Drives the Settings backends
/// editor's status rows.
#[tauri::command]
pub async fn offload_statuses(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<Vec<crate::offload::supervisor::BackendStatus>> {
    Ok(offload_server_use_cases(&supervisor).backend_states().await)
}

/// V8-02: start one named Local backend (idempotent). `command_override`
/// (the Offload server dashboard's "show command on start" popup) launches with
/// that command instead of the configured one for this start only — it goes
/// through the same parse/validation and is never persisted.
#[tauri::command]
pub async fn offload_backend_start(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
    command_override: Option<String>,
) -> AppResult<()> {
    offload_server_use_cases(&supervisor)
        .start_backend(&name, command_override)
        .await
}

/// V8-02: stop one named Local backend (idempotent).
#[tauri::command]
pub async fn offload_backend_stop(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    offload_server_use_cases(&supervisor).stop_backend(&name).await;
    Ok(())
}

/// V8-02: restart (Reset) one named Local backend.
#[tauri::command]
pub async fn offload_backend_restart(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    offload_server_use_cases(&supervisor).restart_backend(&name).await
}

/// Start the offload `llama-server` (idempotent).
#[tauri::command]
pub async fn offload_server_start(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    offload_server_use_cases(&supervisor).start_server().await
}

/// Stop the offload `llama-server` (idempotent).
#[tauri::command]
pub async fn offload_server_stop(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    offload_server_use_cases(&supervisor).stop_server().await;
    Ok(())
}

/// Reset: kill + respawn with the current `server_command` (re-health,
/// re-read `n_ctx`/`np`).
#[tauri::command]
pub async fn offload_server_restart(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    offload_server_use_cases(&supervisor).restart_server().await
}

/// Run a canned offload task against the local server and return its
/// answer (the Settings "Test offload" button).
#[tauri::command]
pub async fn offload_test(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    instructions: String,
) -> AppResult<String> {
    offload_server_use_cases(&supervisor).run_test(instructions).await
}

/// V21: derive a harness's local-provider block from a Local backend's server
/// command (the Settings "Add to OpenCode" button). Pure — parses and validates
/// only; the frontend persists the returned snapshot via `settings_update`. On a
/// missing `--port` or model flag it errors with a message naming exactly what's
/// absent, which the button surfaces verbatim.
///
/// **V40 Phase E (locked decision 26).** The body used to call
/// `offload::server::derive_opencode_provider` — core holding one harness's
/// config writer. It asks the registry now, through
/// [`crate::harness::plugin::ConfigWriter`].
///
/// `harness` is optional for wire compatibility with the pre-V40 frontend: when
/// it is absent the registry answers, and it **refuses if more than one harness
/// declares a writer** rather than picking the first. A silently-chosen harness
/// here would write one product's provider block into another's settings, which
/// is precisely the class of defect this milestone removes. The Settings section
/// passes the id explicitly once decision 27 lands.
#[tauri::command]
pub async fn offload_derive_local_provider(
    harness: Option<String>,
    server_command: String,
) -> AppResult<crate::settings::LocalProviderBlock> {
    crate::service::offload::derive_local_provider(harness.as_deref(), &server_command)
}

/// V8-03: aggregate offload-service status — the honest global in-flight
/// count (now that the long-lived app sees every offload) and per-MCP-server
/// health rows. Drives the Settings warm-pool readout.
#[tauri::command]
pub async fn offload_service_status(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<crate::offload::service::ServiceStatus> {
    Ok(offload_service_use_cases(&service).aggregate_state().await)
}

/// Reconcile the warm MCP host against the *current* settings and return the
/// fresh status. The Settings MCP editor calls this right after persisting an
/// add/remove/enable/disable so a server connects or drops live — no app
/// restart. Cheap when the pool is already warm (unchanged servers are kept).
#[tauri::command]
pub async fn offload_reload_mcp(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<crate::offload::service::ServiceStatus> {
    Ok(offload_service_use_cases(&service).reload_mcp().await)
}

/// V32 Phase C/C3: how much of the injection-detection surface is actually live
/// — signature rule files loaded/failed, whether the classifier's weights are
/// installed, and (C3) the updater's installed/available versions, last check
/// and per-component modes. Drives the
/// Settings → Injection protection → Injection detection readout.
///
/// `reload = true` recompiles the rules from disk first, which is what the
/// "Reload rules" button calls after the user edits a file in
/// `detection/rules.d/local/`. Both paths do blocking file I/O and (on reload) a
/// YARA compile, so they run on the blocking pool rather than the async
/// runtime's worker.
#[tauri::command]
pub async fn detection_status(
    state: State<'_, AppState>,
    reload: bool,
) -> AppResult<crate::offload::detection::DetectionStatus> {
    crate::service::offload::detection_status(state.settings.current(), reload).await
}

/// V32 Phase C3: run an update check right now for one component (or both when
/// `component` is omitted), returning the refreshed detection status.
///
/// `apply = true` is the Settings "Apply" button: it overrides a `check-only`
/// mode for this one run so an explicit click can take an offered update
/// without the user flipping a setting and waiting for a tick. It never
/// overrides `off` — a component the user turned off stays off.
///
/// The whole run (network + validation + swap) is awaited, because the caller
/// is a button whose next action is to re-render the result.
///
/// Refused outright when the detection feature does not resolve on (#48). The
/// gate lives HERE and not only in the Svelte `disabled` attribute: a disabled
/// button is a courtesy, and an IPC command is a capability — one is a hint,
/// the other is the enforcement.
#[tauri::command]
pub async fn detection_check_now(
    state: State<'_, AppState>,
    component: Option<String>,
    apply: bool,
) -> AppResult<crate::offload::detection::DetectionStatus> {
    crate::service::offload::detection_check_now(
        &state.settings.current(),
        component.as_deref(),
        apply,
    )
    .await
}

/// V32 Phase C3: restore a component's retained previous version — the Settings
/// Revert button. Blocking (file moves plus a YARA recompile or an `ort`
/// session rebuild), so it runs on the blocking pool.
///
/// Gated exactly like [`detection_check_now`] (#48): with the detection feature
/// off, the updater does not swap bundles in either direction.
#[tauri::command]
pub async fn detection_revert(
    state: State<'_, AppState>,
    component: String,
) -> AppResult<crate::offload::detection::DetectionStatus> {
    crate::service::offload::detection_revert(&state.settings.current(), &component).await
}

/// V32 Phase C3: open `<exe-dir>/detection/rules.d/` in the host file manager,
/// creating it first so the call does not fail on a layout where the folder was
/// never staged. Same shape as [`content_open_folder`] — one pattern for "show
/// me this directory".
///
/// **Left as a direct call** (V42 Phase A): the body is a `create_dir_all` and
/// a platform `cfg!` handing a cImp-computed path to the host file manager —
/// there is nothing to assert here that does not assert `explorer.exe`. And
/// [`spawn_ledger`](crate::spawn_ledger)'s row of record names THIS file as the
/// spawn site of both folder openers; moving an audited spawn to a service that
/// buys no testability would move the audit for nothing.
#[tauri::command]
pub async fn detection_open_rules_folder() -> AppResult<()> {
    let dir = crate::offload::detection::signature::rules_dir().ok_or_else(|| {
        AppError::Settings("the rules directory could not be resolved".to_string())
    })?;
    if let Err(e) = std::fs::create_dir_all(dir.join("local")) {
        return Err(AppError::Settings(format!(
            "create_dir_all {}: {e}",
            dir.display()
        )));
    }
    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let result = if cfg!(target_os = "windows") {
        crate::spawn_gate::spawn_std(std::process::Command::new("explorer").arg(&dir))
    } else if cfg!(target_os = "macos") {
        crate::spawn_gate::spawn_std(std::process::Command::new("open").arg(&dir))
    } else {
        crate::spawn_gate::spawn_std(std::process::Command::new("xdg-open").arg(&dir))
    };
    result
        .map(|_| ())
        .map_err(|e| AppError::Settings(format!("open folder: {e}")))
}

/// V32 Phase F (locked decision 15): every tab's taint-latch state — the input
/// to the per-tab badge and its override popover.
///
/// **IPC, not HTTP, deliberately.** The same rows are served by the loopback's
/// `GET /status`, but every loopback route is bearer-token authenticated so
/// that only cImp-spawned children can reach it; handing that token to the
/// webview to save one command would widen the trust boundary the token exists
/// to draw. The Tauri backend owns the registry in-process, so this reads it
/// directly.
///
/// Cheap by construction — a couple of mutexes, a handful of `(agent, tab)`
/// entries, no I/O — which is what makes the UI's short poll interval
/// acceptable.
///
/// Step 4 gives it one side effect, deliberately: it folds each tab's current
/// live session into the latch registry before reading it, so a session rotation
/// the harness has already proved is *observed* on this poll rather than
/// whenever the model next calls a cImp tool. That matters only for a tab whose
/// user armed the one-shot contamination clear by restoring a checkpoint — see
/// `latch::TabLatch::awaiting_session_clear`. It grants nothing the next
/// gated call would not have granted anyway; it only decides when the same fact
/// becomes visible.
///
/// **Left as a direct call** (V42 Phase A): one call on the `AppHandle` Tauri
/// injected, and the reason it needs one — the latch scope resolves through the
/// V28 live-session registry — is the seam V42 #114 (loopback: latch +
/// discovery) exists to unpick. A service in front of it would have to be
/// undone by that work. Same for [`latch_override`] and [`injection_status`].
#[tauri::command]
pub async fn latch_status(
    app: tauri::AppHandle,
) -> AppResult<Vec<crate::offload::latch::LatchStatus>> {
    Ok(crate::offload::latch::latch_snapshot(&app))
}

/// V32 Phase G (locked decision 16): the RESOLVED state of every injection
/// control at every scope, plus which of the three levels decided each one.
///
/// The same object the loopback's `GET /status` carries under `injection`, so
/// the Settings matrix, the per-tab badge popover, the status-bar indicator and
/// a live-verification `curl` all read one description of what is in force.
/// Introspection is part of the feature, not a debug affordance: with three
/// levels, "why is this tab not latching?" must be answerable without reading
/// code.
///
/// **Left as a direct call**, for [`latch_status`]'s reason: the resolver is
/// `offload::loopback`'s, and #114 owns where it lives.
#[tauri::command]
pub async fn injection_status(state: State<'_, AppState>) -> AppResult<serde_json::Value> {
    Ok(crate::offload::loopback::injection_status(
        &state.settings.current(),
    ))
}

// There is deliberately no `set_injection_override` command: the L1/L2 switches
// and the L3 override cells are ordinary settings fields, written through the
// normal `apply_settings` save path, so the Settings window keeps ONE write path
// and cannot race its own full-object save against a side channel. What has no
// ordinary path — and is therefore what `injection_status` exists for — is the
// RESOLVED view: it is derived, not stored.
//
// The "this feature has no cell at that scope" case is guarded structurally
// rather than at an IPC boundary: `TabInjectionOverrides` and
// `WorkerInjectionOverrides` carry only their own scope's fields, so the illegal
// write does not typecheck in Rust and has no key to target in JSON.

/// V32 Phase F (locked decision 15): apply a user-initiated containment move to
/// one tab and return its new view — the two latch moves (`"flip_local"`,
/// `"unlatch"`) and step 4's two contamination moves (`"clear_contamination"`,
/// `"await_session_clear"`).
///
/// **This is the only path that can release a contamination flag**, and since
/// decision 15's 2026-08-10 amendment `"unlatch"` releases one too (restoring
/// FULL access is the user's verdict; `"flip_local"` is a workflow step and
/// keeps the flag). See `latch::TabLatch::contaminated` for why a click in
/// this app's own UI is a legitimate trust root where a transcript file is not.
///
/// Errors carry a human-readable reason (unknown action, no latch to move, an
/// illegal transition) that the popover shows verbatim — a security control
/// that silently does nothing when clicked teaches the user to distrust it.
///
/// The `AppHandle` is needed because the latch scope is resolved through the
/// V28 live-session registry, exactly as a gated tool call resolves it: an
/// override must apply to the conversation the tab is running NOW, not to a
/// stale row left by a previous session.
///
/// **Left as a direct call**, for [`latch_status`]'s reason — and one more
/// here: `offload::loopback`'s route scan asserts that the only caller of
/// `apply_latch_override` is this command, by the exact spelling of the call
/// below. That tripwire is the record of "the clearing path is not an HTTP
/// door", so the call site does not move on a mechanical wrap.
#[tauri::command]
pub async fn latch_override(
    app: tauri::AppHandle,
    tab: String,
    consumer: String,
    action: String,
) -> AppResult<crate::offload::latch::LatchView> {
    crate::offload::latch::apply_latch_override(&app, &consumer, &tab, &action)
        .map_err(AppError::Offload)
}

/// V8-03: buffered `llama-server` output for a backend (primary when `name`
/// is omitted) — the read-only Settings log panel's initial fill. Live lines
/// arrive separately via the `offload-server-output` event.
#[tauri::command]
pub async fn offload_server_log(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: Option<String>,
) -> AppResult<Vec<String>> {
    Ok(offload_server_use_cases(&supervisor).server_log(name))
}

/// V8-03: latest Offload Server dashboard snapshot — one row per enabled
/// backend (Local + Remote), each with slots, throughput, queue, context, and
/// request history. The initial fill for the dashboard; live updates arrive
/// via the `offload-server-metrics` event. Empty before the first poll.
#[tauri::command]
pub async fn offload_server_metrics(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<Vec<crate::offload::metrics::BackendDashboard>> {
    Ok(offload_service_use_cases(&service).server_metrics())
}

/// V22 Phase D: detect the project's languages/tooling and return `run_check`
/// proposals. See [`service::checks::detect`](crate::service::checks::detect) —
/// this is the wire boundary only: it names the warm index the detector asks
/// for its per-language file counts.
#[tauri::command]
pub async fn checks_detect(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::checks::detect::Proposal>> {
    crate::service::checks::detect(root, service.inner()).await
}

/// V22 Phase D: merge selected proposal checks into the project's `checks`
/// setting. See [`ChecksService::apply_proposals`]. `root` is informational:
/// the write targets the active project's settings handle (cImp's settings are
/// the launch project's overlay).
#[tauri::command]
pub async fn checks_apply_proposals(
    state: State<'_, AppState>,
    root: Option<String>,
    checks: Vec<crate::checks::CheckDef>,
) -> AppResult<ApplySummary> {
    let _ = root;
    checks_service(&state).apply_proposals(checks)
}

/// Build the checks service over this app's handles. One place, so no command
/// can drift in what it hands it.
fn checks_service(state: &AppState) -> ChecksService<'_> {
    ChecksService::new(&state.settings)
}

/// V22 Phase D: the passive nudge for the Code Intelligence chip. See
/// [`ChecksService::suggestion`].
#[tauri::command]
pub async fn checks_suggestion(
    state: State<'_, AppState>,
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<ChecksSuggestion> {
    checks_service(&state).suggestion(root, service.inner()).await
}

/// V22 Phase D: remember that the user dismissed the suggestion nudge for this
/// project. See [`ChecksService::dismiss_suggestion`].
#[tauri::command]
pub async fn checks_dismiss_suggestion(state: State<'_, AppState>) -> AppResult<()> {
    checks_service(&state).dismiss_suggestion()
}

/// V22 Phase E: dry-run one (possibly unsaved) `CheckDef` for the Settings
/// "Test" button, in the same OS sandbox a real `run_check` gets. See
/// [`ChecksService::test`]. `state` is here only to reach the live settings;
/// the frontend's invoke arguments are unchanged.
#[tauri::command]
pub async fn checks_test(
    state: State<'_, AppState>,
    root: Option<String>,
    def: crate::checks::CheckDef,
) -> AppResult<crate::checks::ChecksTestResult> {
    checks_service(&state).test(root, def).await
}

/// V22 Phase C/E: validate a `regex-custom` pattern for the ChecksEditor's live
/// (debounced) feedback. See
/// [`service::checks::validate_pattern`](crate::service::checks::validate_pattern).
#[tauri::command]
pub async fn checks_validate_pattern(pattern: String) -> Result<(), String> {
    crate::service::checks::validate_pattern(&pattern)
}

/// Resolve an optional `root` IPC argument to a project directory: the given
/// path when non-blank, else the app's launch directory — the rule itself is
/// [`crate::service::project_root`], and this is its name at the wire boundary.
///
/// It was the graph commands' shared fallback; those resolve inside
/// [`CodeIntelService`] now, so what is left here are the two compose-template
/// commands that documented themselves as "mirroring `graph_rebuild`" and still
/// do. (The duplicated first paragraph this replaces was a copy-paste from the
/// A1 settings run.)
fn resolve_graph_root(root: Option<String>) -> AppResult<std::path::PathBuf> {
    crate::service::project_root(root)
}

/// Build the code-graph use cases over this app's handle. One place, so no
/// command can drift in what it hands them.
fn code_intel_service(
    service: &std::sync::Arc<crate::graph::GraphService>,
) -> CodeIntelService<'_> {
    CodeIntelService::new(service)
}

/// V9-01: known per-root code-graph status (idle/building/ready/error + row
/// counts). The initial fill for the graph status surface; live transitions
/// arrive via the `graph-status` event. Empty before the first build.
///
/// **Left as a direct call** (V42 Phase A): the whole body is one accessor on
/// the handle Tauri already injected — no argument shaping, no ordering, no
/// error mapping. Wrapping it would add a hop that says nothing
/// `GraphService::statuses` does not.
#[tauri::command]
pub async fn graph_status(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
) -> AppResult<Vec<crate::graph::GraphStatus>> {
    Ok(service.statuses())
}

/// V9-01: trigger a full rebuild of the project's code graph. `root` defaults
/// to the app's launch directory (the project cImp was opened in). Returns
/// immediately — the build runs on a worker thread and reports progress via
/// the `graph-status` event. A no-op when a build for that root is already in
/// flight. The store must be built before the `graph_*` MCP tools have data.
/// See [`CodeIntelService::rebuild`].
#[tauri::command]
pub async fn graph_rebuild(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<()> {
    code_intel_service(&service).rebuild(root)
}

/// Open a native file/folder picker for the Settings "Ignore" editor and
/// return a gitignore-style glob for the selection. `None` when the user
/// cancels. See [`CodeIntelService::ignore_pick`].
#[tauri::command]
pub async fn graph_ignore_pick(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    folder: bool,
) -> AppResult<Option<String>> {
    code_intel_service(&service).ignore_pick(folder).await
}

/// V9-01 Phase G: force a full re-embed of the project's doc chunks (drops the
/// vector store, then backfills). The "Rebuild embeddings" action; no-op when
/// semantic search is off. See [`CodeIntelService::rebuild_embeddings`].
#[tauri::command]
pub async fn graph_rebuild_embeddings(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<()> {
    code_intel_service(&service).rebuild_embeddings(root)
}

/// V9-01: probe the configured embedding endpoint on demand (the monitor tab's
/// "Test connection" action). Returns reachability + the live vector dimension
/// or the exact connection error, without running a full embed backfill.
///
/// **Left as a direct call**, for [`graph_status`]'s reason.
#[tauri::command]
pub async fn graph_test_embedder(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
) -> AppResult<crate::graph::EmbedderProbe> {
    Ok(service.test_embedder().await)
}

/// V9-01: recent graph tool calls (cloud Claude + offload worker), newest
/// first. See [`service::graph::history`](crate::service::graph::history) for
/// the scoping rule and why every store call runs on the blocking pool.
#[tauri::command]
pub async fn graph_history(
    root: Option<String>,
    scoped: Option<bool>,
    since_ts: Option<u64>,
) -> AppResult<Vec<crate::activity::ActivityEntry>> {
    crate::service::graph::history(root, scoped, since_ts).await
}

/// The unified tool-activity feed (graph calls + offload runs), newest first,
/// without payloads — the Tool Activity tab's poll and the #51 Events tab's.
///
/// **Deliberately unfiltered, and the Events tab narrows client-side** (#51).
/// A server-side filter shipped here briefly and was removed: the store is
/// capped per lane at ~1,570 light rows *by construction*, so the payload this
/// avoids cannot grow; the Tool Activity tab has full-polled this same store
/// every couple of seconds since v0.41.0; and the filter bar's option lists
/// have to be derived from an UNFILTERED read anyway, so a narrowed poll would
/// have been a second request beside the full one rather than a replacement.
///
/// What settled it was not the dead code but the duplication: filtering
/// server-side means a second copy of the four-state attribution rule, and only
/// one copy can be the exercised one. That rule — whether an `Unrecognized` id
/// counts as its tab — is the property the whole Events view rests on, and it
/// fails by showing MORE than was asked. One implementation, in the layer that
/// actually runs it.
#[tauri::command]
pub async fn activity_list(since_ts: Option<u64>) -> AppResult<Vec<crate::activity::ActivityEntry>> {
    crate::service::view::activity_since(since_ts).await
}

/// One activity's full record — including the captured request/response
/// payloads — for the detail popup. `None` when the entry was deleted (or
/// aged out) between the list poll and the click.
#[tauri::command]
pub async fn activity_detail(id: u64) -> AppResult<Option<crate::activity::ActivityRecord>> {
    crate::service::view::activity_detail(id).await
}

/// Delete one activity entry (persists immediately).
#[tauri::command]
pub async fn activity_delete(id: u64) -> AppResult<()> {
    crate::service::view::activity_delete(id).await
}

/// Clear the whole activity history (persists immediately).
#[tauri::command]
pub async fn activity_clear() -> AppResult<()> {
    crate::service::view::activity_clear().await
}

/// V10 (Analyses): candidate unused public symbols — public/exported defs with
/// no reference and no inbound call edge. Candidates only; the UI states the
/// false-positive caveat (dynamic dispatch, external API, macros/reflection).
/// `root` defaults to the launch directory. On-demand (no background schedule).
/// See [`CodeIntelService::dead_exports`].
#[tauri::command]
pub async fn graph_dead_exports(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<DeadExportRow>> {
    code_intel_service(&service).dead_exports(root)
}

/// V10 (Analyses): import cycles between files (each a loop of ≥ 2 files that
/// transitively import one another). `root` defaults to the launch directory.
/// See [`CodeIntelService::cycles`].
#[tauri::command]
pub async fn graph_cycles(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<Vec<String>>> {
    code_intel_service(&service).cycles(root)
}

/// V12 Phase B (Analyses): "what does my current working-tree change affect?" —
/// diff mode only (the `symbols`-scoped mode is MCP-tool only, where an agent
/// supplies explicit roots). `root` defaults to the launch directory. Errors
/// with a "requires git" message when `root` isn't a git repository (see
/// `AppError::NotAGitRepo`). See [`CodeIntelService::impact`].
#[tauri::command]
pub async fn graph_impact(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<ImpactResult> {
    code_intel_service(&service).impact(root)
}

/// V15 Feature 1 (Architecture): trace the shortest path between two entities
/// through the call/import/containment graph. `root` defaults to the launch
/// directory. See [`CodeIntelService::path`].
#[tauri::command]
pub async fn graph_path(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    from: String,
    to: String,
    kinds: Option<Vec<String>>,
    symmetric: Option<bool>,
) -> AppResult<PathResult> {
    code_intel_service(&service).path(root, &from, &to, kinds, symmetric)
}

/// V15 Feature 2 (Architecture): the system-shape overview — god nodes,
/// subsystems, and surprising cross-subsystem edges. `root` defaults to the
/// launch directory. See [`CodeIntelService::architecture`].
#[tauri::command]
pub async fn graph_architecture(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<ArchResult> {
    code_intel_service(&service).architecture(root)
}

/// V15 Feature 4 (Graph View): a bounded {nodes, edges} subgraph for the live
/// visualization (Tool Activity → Graph view). `root` defaults to the launch
/// directory. See [`CodeIntelService::viz_snapshot`].
#[tauri::command]
pub async fn graph_viz_snapshot(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<VizGraphResult> {
    code_intel_service(&service).viz_snapshot(root)
}

/// Workbench ⌖ support: per-file Graph View presence for a batch of
/// repo-relative paths — the jump button disables for unindexed or
/// connection-less files. `root` defaults to the launch directory. See
/// [`CodeIntelService::viz_file_status`].
#[tauri::command]
pub async fn graph_viz_file_status(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    paths: Vec<String>,
) -> AppResult<Vec<VizFileStatusRow>> {
    code_intel_service(&service).viz_file_status(root, &paths)
}

/// Workbench ⌖ support: the 1-hop FILE ego of `path` regardless of the
/// snapshot's top-N-by-degree cut — the Graph View injects it temporarily when a
/// jump targets a file the rendered snapshot dropped. `root` defaults to the
/// launch directory. See [`CodeIntelService::viz_ego`].
#[tauri::command]
pub async fn graph_viz_ego(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    path: String,
) -> AppResult<VizGraphResult> {
    code_intel_service(&service).viz_ego(root, &path)
}

/// V10 (Memory): the project's session/action memory — current session, its
/// working set, notes (pinned + current-session), and the recent-sessions list.
/// `root` defaults to the launch directory. See [`CodeIntelService::memory`].
#[tauri::command]
pub async fn graph_memory(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<crate::graph::MemorySnapshot> {
    code_intel_service(&service).memory(root)
}

/// V10 (Memory): clear one session's memory (`session` = its id) or the whole
/// project's memory (`session` omitted). `root` defaults to the launch
/// directory. See [`CodeIntelService::memory_clear`].
#[tauri::command]
pub async fn graph_memory_clear(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    session: Option<String>,
) -> AppResult<()> {
    code_intel_service(&service).memory_clear(root, session)
}

/// V10 (Memory): pin/unpin a note (pinned notes survive session eviction and
/// show project-wide). `root` defaults to the launch directory. See
/// [`CodeIntelService::note_set_pinned`].
#[tauri::command]
pub async fn graph_note_set_pinned(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    note_id: String,
    pinned: bool,
) -> AppResult<()> {
    code_intel_service(&service).note_set_pinned(root, &note_id, pinned)
}

/// V32 Phase C2 (Memory): resolve one QUARANTINED note — `action` is
/// `"promote"` or `"discard"`. `root` defaults to the launch directory. See
/// [`CodeIntelService::note_review`] for why an unknown action is rejected
/// rather than ignored.
#[tauri::command]
pub async fn graph_note_review(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    note_id: String,
    action: String,
) -> AppResult<()> {
    code_intel_service(&service).note_review(root, &note_id, &action)
}

/// V12 Phase E (Memory): the project's durable facts (pinned first, then
/// newest), excluding archived ones. `root` defaults to the launch directory.
/// See [`CodeIntelService::facts`].
#[tauri::command]
pub async fn graph_facts(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::graph::ProjectFact>> {
    code_intel_service(&service).facts(root)
}

/// V12 Phase E (Memory): pin / unpin / archive / delete one project fact.
/// `root` defaults to the launch directory. See
/// [`CodeIntelService::fact_update`].
#[tauri::command]
pub async fn graph_fact_update(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    id: String,
    action: String,
) -> AppResult<()> {
    code_intel_service(&service).fact_update(root, &id, &action)
}

/// V12 Phase E (Memory): manually add a project fact from the Facts UI's "add
/// fact" input (recorded with `source_session = "manual"`). `root` defaults to
/// the launch directory. See [`CodeIntelService::fact_add`].
#[tauri::command]
pub async fn graph_fact_add(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    text: String,
    pin: Option<bool>,
) -> AppResult<()> {
    code_intel_service(&service).fact_add(root, &text, pin)
}

/// V10 (Context): preview what context injection WOULD prepend for `prompt`,
/// bypassing the `context_injection` toggle (so the user can tune before
/// enabling). Requires the graph to be enabled. `root` defaults to the launch
/// directory; no `session_id` (the preview isn't tied to a live session). See
/// [`CodeIntelService::context_preview`].
#[tauri::command]
pub async fn graph_context_preview(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    prompt: String,
    root: Option<String>,
) -> AppResult<crate::graph::RetrieveResult> {
    code_intel_service(&service).context_preview(&prompt, root)
}

// ── V14 Phase D/D2: Usage section (token X-ray) + budget-tuning advisor ───

/// Build the Usage/Advisor use cases over this app's handle. One place, so no
/// command can drift in what it hands them.
fn usage_service(service: &std::sync::Arc<crate::graph::GraphService>) -> UsageService<'_> {
    UsageService::new(service)
}

/// V14 Phase D: the Usage section's full payload for `root` — the current
/// session's per-turn series + top-tools ranking, every known session's totals
/// row, and the effectiveness counters. `root` defaults to the launch
/// directory. See [`UsageService::snapshot`], which also says why the pass runs
/// on the blocking pool. This is the wire boundary only: it names the offload
/// pool, whose local-task count `GraphService` cannot fill.
#[tauri::command]
pub async fn graph_usage(
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    offload: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
    root: Option<String>,
) -> AppResult<crate::graph::UsageSnapshot> {
    usage_service(&graph).snapshot(root, offload.inner().clone()).await
}

/// V24 Phase B: full drill-in detail for ONE session under `root` — its totals
/// row, per-turn series, top-tools ranking, and per-model token totals with the
/// session/agent origin split. An unknown session id returns an empty detail
/// (no error, no panic). `root` defaults to the launch directory. See
/// [`UsageService::session_detail`].
#[tauri::command]
pub async fn graph_session_usage(
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    session_id: String,
) -> AppResult<crate::graph::SessionUsageDetail> {
    usage_service(&graph).session_detail(root, session_id).await
}

/// V34: which session the tab keyed `tab` is currently working in, or `null`
/// when the app cannot prove one.
///
/// This is what lets the Code Intelligence Overview follow the focused agent
/// tab instead of always rendering the most-recently-active session — with two
/// Claude tabs open, "most recent" is whichever tab last wrote, not the one the
/// user is looking at. `null` is the honest answer for an unpinned tab sharing a
/// project with a co-tenant (V28 decision 4a), a tab that has not started, or a
/// non-agent tab; the caller falls back to its previous behaviour rather than
/// showing a session it cannot attribute.
///
/// **Left as a direct call** (V42 Phase A), for [`graph_status`]'s reason: the
/// whole body is one accessor on the handle Tauri already injected.
#[tauri::command]
pub async fn graph_tab_session(
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    tab: String,
) -> AppResult<Option<String>> {
    Ok(graph.live_session_for_any_agent(&tab))
}

/// V14 Phase D2: the budget-tuning advisor's current proposals for `root`.
/// `root` defaults to the launch directory. See [`UsageService::advice`] for
/// the ~25 signals it assembles and why the whole pass runs off the runtime
/// workers.
#[tauri::command]
pub async fn graph_usage_advice(
    state: State<'_, AppState>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<AdvisorSnapshot> {
    usage_service(&graph).advice(&state.settings, root).await
}

/// **Every registered harness, as the window sees it** (V40 Phase F, locked
/// decisions 7, 11 and 27).
///
/// The one command the frontend learns the roster from: ids, labels, reserved
/// tab ids, binaries, features, consumer token, the declared `ext` fields and
/// the affordance strings. See [`crate::harness::info`] for the shape and for
/// the committed fixture that keeps the TypeScript mirror honest.
///
/// Deliberately a SEPARATE command from [`harness_versions_get`], unlike the
/// health panel that shares it: this answer is `'static` data that cannot go
/// stale between calls, so there is no consistency argument for folding it in,
/// and each window fetches it once at startup rather than on every poll.
///
/// It subsumes Phase B's `harness_settings_schema`, exactly as that command's
/// doc comment said it would — the declared fields are one more column of the
/// same row, and two commands would have meant two round trips the window had
/// to keep in step.
///
/// **Left as a direct call** (V42 Phase A): the whole body is one call on a
/// `'static` table — no handle, no argument shaping, nothing a service could
/// hold.
#[tauri::command]
pub async fn harness_list() -> AppResult<Vec<crate::harness::info::HarnessInfo>> {
    Ok(crate::harness::info::harness_list())
}

/// Build the harness-administration use cases over this app's handle. One
/// place, so no command can drift in what it hands them.
fn harness_service(state: &AppState) -> HarnessService<'_> {
    HarnessService::new(&state.settings)
}

/// V16 Feature 1: the harness version + contract-verification state, read from
/// the physical global `settings.json` (fresh — background writers bypass the
/// live settings snapshot). See [`HarnessService::versions`] for the three
/// different freshness contracts this cluster deliberately keeps apart.
#[tauri::command]
pub async fn harness_versions_get(state: State<'_, AppState>) -> AppResult<HarnessStatus> {
    harness_service(&state).versions()
}

/// V35 Phase G: the *Harness health* panel's one action — run this harness's L1
/// canaries and L2 probes now. Returns whether a run STARTED. See
/// [`service::harness::run_checks`](crate::service::harness::run_checks).
#[tauri::command]
pub async fn harness_run_checks(harness: String) -> AppResult<bool> {
    crate::service::harness::run_checks(&harness)
}

/// V16 Feature 1: the Advisor card's "Mark verified" action — stamp the
/// currently-seen version of `harness` as the last-verified one. `None` is the
/// DEFAULT harness (locked decision 22's wire-compatibility default). See
/// [`HarnessService::mark_verified`].
#[tauri::command]
pub async fn harness_mark_verified(
    state: State<'_, AppState>,
    harness: Option<String>,
) -> AppResult<()> {
    harness_service(&state).mark_verified(harness)
}

/// **The model-visible text one tab's harness receives**, keyed by slot (V40
/// Phase E, locked decision 24). A tab that runs no registered harness (or an
/// unknown id) gets the NEUTRAL rendering, which is a real answer rather than a
/// failure. See [`HarnessService::instructions`].
#[tauri::command]
pub async fn harness_instructions(
    state: State<'_, AppState>,
    tab: Option<String>,
) -> AppResult<std::collections::BTreeMap<String, String>> {
    harness_service(&state).instructions(tab)
}

/// **The advisor's rule reference** (V40 Phase F, locked decision 23).
///
/// The Code Intelligence panel used to hold this table as a hard-coded tooltip
/// — a restatement of thresholds `advisor.rs` owns, with one harness's
/// mechanisms named in it for rules that fire per registered harness. It
/// renders this instead. See
/// [`service::usage::rules`](crate::service::usage::rules).
#[tauri::command]
pub async fn advisor_rules() -> AppResult<AdvisorRules> {
    Ok(crate::service::usage::rules())
}

/// V14 Phase D2: dismiss one advisor proposal (`rule_id` + its coarse rate
/// `signature`, both echoed from the `Proposal` the user clicked Dismiss on).
/// Idempotent — dismissing the same pair twice is a no-op. See
/// [`service::usage::dismiss`](crate::service::usage::dismiss).
#[tauri::command]
pub async fn advisor_dismiss(
    state: State<'_, AppState>,
    rule_id: String,
    signature: String,
) -> AppResult<()> {
    crate::service::usage::dismiss(&state.settings, rule_id, signature)
}

/// Record that the user APPLIED an advisor proposal, starting the rule's Apply
/// cooldown. Called by the Advisor card's Apply right after the
/// `settings_update` that writes the proposed value — the settings write itself
/// stays the ordinary path (never silent self-modification). See
/// [`UsageService::mark_applied`].
#[tauri::command]
pub async fn advisor_mark_applied(
    state: State<'_, AppState>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    rule_id: String,
) -> AppResult<()> {
    usage_service(&graph).mark_applied(&state.settings, root, rule_id)
}

/// Build the Workbench use cases over this app's handle. One place, so no
/// command can drift in what it hands them.
fn workbench_use_cases(
    service: &std::sync::Arc<crate::workbench::WorkbenchService>,
) -> WorkbenchUseCases<'_> {
    WorkbenchUseCases::new(service)
}

/// V13 Phase A: the Workbench tab's top-of-view banner data — is `git` on
/// PATH at all, and is `root` inside a working tree. See
/// [`WorkbenchUseCases::status`].
#[tauri::command]
pub async fn workbench_status(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
) -> AppResult<crate::workbench::WorkbenchStatus> {
    workbench_use_cases(&service).status(root).await
}

/// V13 Phase B: the Diff section's file list — status/binary/too_large per
/// file plus the readonly (mid-merge/-rebase) and source flags. See
/// [`WorkbenchUseCases::diff_summary`].
#[tauri::command]
pub async fn workbench_diff_summary(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
) -> AppResult<crate::workbench::diff::DiffSummary> {
    workbench_use_cases(&service).diff_summary(root).await
}

/// V13 Phase B: one file's full parsed diff (hunks + lines), fetched only
/// when the frontend expands that file's row. `context` is the unified-context
/// width (default 3); the frontend's "full file" toggle passes a huge value,
/// clamped by the service. See [`WorkbenchUseCases::diff_file`].
#[tauri::command]
pub async fn workbench_diff_file(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    path: String,
    context: Option<u32>,
) -> AppResult<crate::workbench::diff::FileDiff> {
    workbench_use_cases(&service)
        .diff_file(root, &path, context)
        .await
}

/// V13 Phase B B2: revert one hunk. `hunk_hash` must match the hash of the
/// hunk currently at `hunk_index` — a mismatch means the file changed since
/// the frontend last fetched it (an agent edit raced the diff view) and the
/// revert is refused rather than applied against stale content. See
/// [`WorkbenchUseCases::revert_hunk`].
#[tauri::command]
pub async fn workbench_revert_hunk(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    path: String,
    hunk_index: usize,
    hunk_hash: String,
) -> AppResult<crate::workbench::diff::FileDiff> {
    workbench_use_cases(&service)
        .revert_hunk(root, &path, hunk_index, &hunk_hash)
        .await
}

/// V13 Phase B: format one hunk as a fenced code block + `path:line` header
/// for the compose overlay's "Send to agent" hunk action. See
/// [`WorkbenchUseCases::send_hunk`].
#[tauri::command]
pub async fn workbench_send_hunk(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    path: String,
    hunk_index: usize,
) -> AppResult<String> {
    workbench_use_cases(&service)
        .send_hunk(root, &path, hunk_index)
        .await
}

/// V13 Phase C: the Timeline section's row list — every checkpoint currently
/// retained in the shadow repo, oldest first. Empty (not an error) when
/// checkpoints have never run for `root`. See
/// [`WorkbenchUseCases::checkpoints`].
#[tauri::command]
pub async fn workbench_checkpoints(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::workbench::shadow::Checkpoint>> {
    workbench_use_cases(&service).checkpoints(root).await
}

/// V13 Phase C: checkpoint `id` vs. the CURRENT working tree — powers both the
/// Timeline's "Diff vs now" viewer and the restore confirmation dialog's
/// dry-run file list. See [`WorkbenchUseCases::checkpoint_diff`].
#[tauri::command]
pub async fn workbench_checkpoint_diff(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    id: String,
    context: Option<u32>,
) -> AppResult<Vec<crate::workbench::diff::FileDiff>> {
    workbench_use_cases(&service)
        .checkpoint_diff(root, &id, context)
        .await
}

/// V13 Phase C: the manual "Checkpoint now" action. `label` defaults to
/// "manual checkpoint" when omitted. Unlike the automatic triggers this is NOT
/// throttled by `checkpoint_min_gap_s`. See
/// [`WorkbenchUseCases::checkpoint_now`].
#[tauri::command]
pub async fn workbench_checkpoint_now(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    label: Option<String>,
) -> AppResult<crate::workbench::shadow::CheckpointId> {
    workbench_use_cases(&service)
        .checkpoint_now(root, label)
        .await
}

/// V13 Phase C: restore the working tree to checkpoint `id`.
/// **Safety-critical**: `delete_new` MUST default to `false` on the frontend
/// (the confirmation dialog's "delete files created since" checkbox starts
/// unchecked) — see `shadow::restore`'s doc comment for the invariants this
/// upholds. See [`WorkbenchUseCases::restore`].
#[tauri::command]
pub async fn workbench_restore(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    id: String,
    delete_new: bool,
) -> AppResult<crate::workbench::shadow::RestoreReport> {
    workbench_use_cases(&service)
        .restore(root, &id, delete_new)
        .await
}

/// V33 step 5: the contamination lifecycle the Workbench Timeline renders
/// beside its checkpoints, plus the root those checkpoints belong to. See
/// [`service::workbench::contamination_events`](crate::service::workbench::contamination_events)
/// for why this is a command of its own rather than `activity_list` + N ×
/// `activity_detail`.
#[tauri::command]
pub async fn contamination_events(root: Option<String>) -> AppResult<serde_json::Value> {
    crate::service::workbench::contamination_events(root).await
}

/// V13 Phase D: every cImp-managed worktree of `root`'s repo — slug, branch,
/// base branch, ahead/behind vs that base, and whether an AI tab is currently
/// pointed at it. See [`WorkbenchUseCases::worktrees`].
#[tauri::command]
pub async fn workbench_worktrees(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::workbench::worktree::WorktreeInfo>> {
    workbench_use_cases(&service).worktrees(root).await
}

/// V13 Phase D D3: worktree `slug` vs. the base branch it was cut from
/// (`git diff <base>...cimp/<slug>`). Read-only — there is no revert action on
/// this diff. See [`WorkbenchUseCases::worktree_diff`].
#[tauri::command]
pub async fn workbench_worktree_diff(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    slug: String,
    context: Option<u32>,
) -> AppResult<Vec<crate::workbench::diff::FileDiff>> {
    workbench_use_cases(&service)
        .worktree_diff(root, &slug, context)
        .await
}

/// Session-commits section: the union of commits caught live from the
/// session's transcript and commits whose committer time falls inside the
/// session's window, newest first. The frontend's `from_ms..=to_ms` is only a
/// fallback snapshot — see [`WorkbenchUseCases::session_commits`] and
/// [`widen`](crate::service::workbench) for the union rule. This is the wire
/// boundary only: it names the code graph as the session bookkeeping source.
#[tauri::command]
pub async fn workbench_session_commits(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    session_id: String,
    from_ms: i64,
    to_ms: i64,
) -> AppResult<crate::workbench::history::SessionCommits> {
    workbench_use_cases(&service)
        .session_commits(root, &session_id, from_ms, to_ms, graph.inner())
        .await
}

/// Per-session commit counts (session_id → count) for the Sessions card's
/// per-row "commits" button — a zero count disables it. Frontend-supplied
/// windows are widened with the graph's own canonical session windows, same as
/// [`workbench_session_commits`]. See
/// [`WorkbenchUseCases::session_commit_counts`].
#[tauri::command]
pub async fn workbench_session_commit_counts(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    windows: Vec<crate::workbench::history::SessionWindow>,
) -> AppResult<std::collections::HashMap<String, u32>> {
    workbench_use_cases(&service)
        .session_commit_counts(root, windows, graph.inner())
        .await
}

/// One commit vs. its first parent — the Session-commits section's
/// expanded-commit file list. Read-only. See
/// [`WorkbenchUseCases::commit_diff`].
#[tauri::command]
pub async fn workbench_commit_diff(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    hash: String,
    context: Option<u32>,
) -> AppResult<Vec<crate::workbench::diff::FileDiff>> {
    workbench_use_cases(&service)
        .commit_diff(root, &hash, context)
        .await
}

/// The Git-graph section: up to `limit` commits from every ref in topological
/// order (children before parents — what the frontend's lane layout needs)
/// plus the current branch name. See [`WorkbenchUseCases::git_graph`].
#[tauri::command]
pub async fn workbench_git_graph(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    limit: Option<usize>,
) -> AppResult<crate::workbench::history::GitGraph> {
    workbench_use_cases(&service).git_graph(root, limit).await
}

/// V13 Phase D: create a bare worktree (no tab) for `slug` — the Worktrees
/// section's own "create" affordance. Returns the new worktree's absolute
/// path. See [`WorkbenchUseCases::worktree_create`], which also holds the
/// tab-lifecycle serializer this hands it and says why.
#[tauri::command]
pub async fn workbench_worktree_create(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    state: State<'_, AppState>,
    root: Option<String>,
    slug: String,
) -> AppResult<String> {
    workbench_use_cases(&service)
        .worktree_create(&state.lifecycle_serializer, root, &slug)
        .await
}

/// V13 Phase D: merge worktree `slug`'s branch back into the branch it was cut
/// from. **Safety-critical** — see `workbench::worktree::merge`'s doc comment:
/// on ANY failure past the preconditions the merge is aborted before this
/// returns. See [`WorkbenchUseCases::worktree_merge`].
#[tauri::command]
pub async fn workbench_worktree_merge(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    slug: String,
) -> AppResult<crate::workbench::worktree::MergeReport> {
    workbench_use_cases(&service)
        .worktree_merge(root, &slug)
        .await
}

/// V13 Phase D: remove worktree `slug`'s directory and delete its branch.
/// **Double-confirmation is the frontend's job** — this call performs the
/// removal unconditionally once invoked. See
/// [`WorkbenchUseCases::worktree_discard`].
#[tauri::command]
pub async fn workbench_worktree_discard(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    slug: String,
) -> AppResult<()> {
    workbench_use_cases(&service)
        .worktree_discard(root, &slug)
        .await
}

/// V13 Phase D D3: the merge-readiness chip's "Run checks" action — runs every
/// configured check with `cwd` = the worktree, caches the aggregate pass/fail,
/// and returns it. See [`WorkbenchUseCases::worktree_run_checks`].
#[tauri::command]
pub async fn workbench_worktree_run_checks(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    slug: String,
) -> AppResult<crate::workbench::WorktreeCheckStatus> {
    workbench_use_cases(&service)
        .worktree_run_checks(root, &slug)
        .await
}

/// V13 Phase D D3: the merge-readiness chip's last cached result for `slug`,
/// if any check has been run this session — `null` on the wire means "not
/// checked yet", not a failure. See
/// [`WorkbenchUseCases::worktree_check_status`].
#[tauri::command]
pub fn workbench_worktree_check_status(
    service: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    root: Option<String>,
    slug: String,
) -> AppResult<Option<crate::workbench::WorktreeCheckStatus>> {
    workbench_use_cases(&service).worktree_check_status(root, &slug)
}

/// V9-01: pause/resume the graph's incremental fs-watcher re-indexing. Paused
/// = file changes are ignored until resumed (a manual rebuild still works).
///
/// **Left as a direct call** (V42 Phase A), for [`graph_status`]'s reason: the
/// whole body is one accessor on the handle Tauri already injected.
#[tauri::command]
pub async fn graph_set_watch_paused(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    paused: bool,
) -> AppResult<bool> {
    Ok(service.set_watch_paused(paused))
}

/// V9-02: the project's language census for the Code Graph tab's language
/// buttons — every language present on disk with its file count and
/// green/yellow/red classification (indexed / supported-but-off / unsupported).
/// `root` defaults to the launch directory. Walks the tree fresh each call, so
/// the frontend calls it on tab open and after a rebuild, not on a poll. See
/// [`CodeIntelService::language_census`].
#[tauri::command]
pub async fn graph_language_census(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::graph::LangCensus>> {
    code_intel_service(&service).language_census(root)
}

/// V9-02: add or remove a language from the code graph's index set. Adds/removes
/// the tag in `GraphSettings.languages` (persisted), then kicks a full rebuild
/// so the change takes effect. Rejects unsupported tags. `root` defaults to the
/// launch directory. See [`CodeIntelService::set_language_enabled`] — this is
/// the wire boundary only: it names the settings handle the toggle writes
/// through.
#[tauri::command]
pub async fn graph_set_language_enabled(
    state: State<'_, AppState>,
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    lang: String,
    enabled: bool,
    root: Option<String>,
) -> AppResult<()> {
    code_intel_service(&service).set_language_enabled(&state.settings, &lang, enabled, root)
}

/// Open `<portable-root>/logs/content/` in the host file manager. Creates the
/// folder first if it doesn't exist so the call doesn't 404 on a clean
/// install. Windows uses `explorer.exe`; macOS `open`; Linux
/// `xdg-open`. Errors are wrapped in `AppError::Settings` for a single
/// IPC error type.
///
/// **Left as a direct call**, for [`detection_open_rules_folder`]'s reason:
/// nothing to assert that does not assert `explorer.exe`, and
/// [`spawn_ledger`](crate::spawn_ledger)'s row of record names this file as the
/// spawn site.
#[tauri::command]
pub async fn content_open_folder() -> AppResult<()> {
    let dir = crate::content::dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(AppError::Settings(format!(
            "create_dir_all {}: {e}",
            dir.display()
        )));
    }
    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let result = if cfg!(target_os = "windows") {
        crate::spawn_gate::spawn_std(std::process::Command::new("explorer").arg(&dir))
    } else if cfg!(target_os = "macos") {
        crate::spawn_gate::spawn_std(std::process::Command::new("open").arg(&dir))
    } else {
        crate::spawn_gate::spawn_std(std::process::Command::new("xdg-open").arg(&dir))
    };
    result
        .map(|_| ())
        .map_err(|e| AppError::Settings(format!("open folder: {e}")))
}

/// Delete every file inside `<portable-root>/logs/content/`. Returns the
/// count of removed files. Per-file failures are logged backend-side
/// and do not abort the pass.
///
/// **Left as a direct call** (V42 Phase A): one call on
/// [`crate::content::delete_all`], which owns the keep-going-on-failure rule.
#[tauri::command]
pub async fn content_clear() -> AppResult<u32> {
    Ok(crate::content::delete_all())
}

/// The voice names the settings picker offers. See
/// [`service::audio::voices`](crate::service::audio::voices) for why a missing
/// voice directory is an empty list rather than an error.
#[tauri::command]
pub async fn list_voices() -> AppResult<Vec<String>> {
    crate::service::audio::voices()
}

/// Open the Settings window, or focus it if it is already open.
///
/// **Left as a direct call** (V42 Phase A): one call on the window helper. The
/// two commands below are the same kind of thing — a host effect on a window,
/// with no argument shaping, no ordering and nothing to return. A service in
/// front of `DwmSetWindowAttribute` would be `AppHandle` with extra steps,
/// which is the reason [`WebviewHost`](crate::service::sink::WebviewHost) is
/// one method wide. The deep-link trio below is wrapped, because that one is a
/// protocol rather than an effect.
#[tauri::command]
pub async fn open_settings_window(app: AppHandle) -> AppResult<()> {
    open_or_focus_settings(&app)
}

/// Close the Settings window if it is open.
///
/// **Left as a direct call**, for [`open_settings_window`]'s reason.
#[tauri::command]
pub async fn close_settings_window(app: AppHandle) -> AppResult<()> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.close();
    }
    Ok(())
}

/// Square off (or restore) the main window's corners. Windows 11 rounds
/// borderless windows via DWM regardless of CSS; the `tui-*` themes drop
/// the native decorations and want hard corners to match the ratatui
/// look, so the frontend calls this with `square = true` when a TUI theme
/// is active and `false` (default OS rounding) otherwise. No-op on
/// non-Windows platforms.
///
/// **Left as a direct call**, for [`open_settings_window`]'s reason: the body
/// is one `DwmSetWindowAttribute` behind a `cfg`, and nothing about it is
/// checkable without a window manager.
#[tauri::command]
pub fn set_window_square_corners(app: AppHandle, square: bool) -> AppResult<()> {
    #[cfg(windows)]
    {
        use tauri::Manager;
        let window = app
            .get_webview_window("main")
            .ok_or_else(|| AppError::Ipc("main window not found".into()))?;
        let hwnd = window
            .hwnd()
            .map_err(|e| AppError::Ipc(format!("hwnd: {e}")))?;

        // DWMWA_WINDOW_CORNER_PREFERENCE = 33; DWMWCP_DEFAULT = 0,
        // DWMWCP_DONOTROUND = 1. Declared inline so we don't pull in the
        // whole `windows` crate for a single attribute call.
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_DEFAULT: u32 = 0;
        const DWMWCP_DONOTROUND: u32 = 1;
        #[link(name = "dwmapi")]
        extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: isize,
                attr: u32,
                pv: *const core::ffi::c_void,
                cb: u32,
            ) -> i32;
        }
        let pref: u32 = if square {
            DWMWCP_DONOTROUND
        } else {
            DWMWCP_DEFAULT
        };
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as isize,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        if hr < 0 {
            return Err(AppError::Ipc(format!(
                "DwmSetWindowAttribute failed: 0x{hr:08x}"
            )));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (app, square);
    }
    Ok(())
}

/// Build the Settings deep-link use cases over this app's one-shot slot.
fn settings_deep_link(state: &AppState) -> SettingsDeepLink<'_> {
    SettingsDeepLink::new(&state.pending_settings_deep_link)
}

/// The real [`SettingsWindow`]: a Tauri app handle plus the label
/// `ipc::windows` builds that window under.
struct TauriSettingsWindow(AppHandle);

impl TauriSettingsWindow {
    fn new(app: AppHandle) -> Self {
        Self(app)
    }
}

impl SettingsWindow for TauriSettingsWindow {
    fn open_or_focus(&self) -> AppResult<()> {
        open_or_focus_settings(&self.0)
    }

    fn label(&self) -> &str {
        SETTINGS_LABEL
    }
}

/// V1.4-07 A: open the Settings window scrolled to a specific tab's
/// section. The right-click "Configure tab" entry on AI tabs uses this
/// instead of the shell-only `ConfigureTabDialog.svelte`. Cold-open is
/// handled by storing the target id in `AppState.pending_settings_deep_link`
/// (the Settings window calls `consume_settings_deep_link` on mount);
/// hot-open by emitting a `settings-deep-link` event the Settings window
/// listens for. We do both so either path works without a race.
#[tauri::command]
pub async fn open_settings_window_to_tab(
    app: AppHandle,
    state: State<'_, AppState>,
    tab: String,
) -> AppResult<()> {
    settings_deep_link(&state).to_tab(
        &TauriSettingsWindow::new(app.clone()),
        &TauriEventSink::new(app),
        &tab,
    )
}

/// V22 Phase E: open the Settings window scrolled to a top-level sidebar
/// section (not a tab). Used by the Code Intelligence "suggested checks" nudge
/// chip to jump straight to the `checks` editor. Reuses the same cold/hot deep
/// link plumbing as [`open_settings_window_to_tab`], tagging the stored target
/// with a `section:` prefix so `SettingsApp`'s consume path routes it to
/// `activeSection` instead of a tab scroll.
#[tauri::command]
pub async fn open_settings_window_to_section(
    app: AppHandle,
    state: State<'_, AppState>,
    section: String,
) -> AppResult<()> {
    settings_deep_link(&state).to_section(
        &TauriSettingsWindow::new(app.clone()),
        &TauriEventSink::new(app),
        &section,
    )
}

/// V1.4-07 A: pulled by `SettingsApp.svelte` on mount to read+clear any
/// pending deep-link target stored by `open_settings_window_to_tab`.
/// Returns `None` when no target is pending.
#[tauri::command]
pub async fn consume_settings_deep_link(state: State<'_, AppState>) -> AppResult<Option<String>> {
    Ok(settings_deep_link(&state).take())
}

/// Trigger a tab restart from another window (typically settings). The
/// Terminal component for the targeted tab owns the channel and sizing —
/// it does the actual `pty_restart` invocation. Routed as a frontend event
/// so the main window can keep all PTY-touching IPC in one place.
#[tauri::command]
pub async fn request_tab_restart(app: AppHandle, tab: TabId) -> AppResult<()> {
    crate::service::tabs::request_tab_restart(&TauriEventSink::new(app), tab, false)
}

/// Restart a closed Shell tab. Driven by the closed-state overlay's
/// Enter-to-restart affordance (Phase 7). Reuses the existing
/// `tab-restart-requested` plumbing so the frontend Terminal can rebind
/// the bytes channel exactly as it does for the settings-window restart
/// path. The state manager clears the closed flag on the subsequent
/// `ShellRestarted` signal emitted from `TabRegistry::restart_tab`.
#[tauri::command]
pub async fn restart_shell_tab(app: AppHandle, tab: TabId) -> AppResult<()> {
    crate::service::tabs::request_tab_restart(&TauriEventSink::new(app), tab, true)
}

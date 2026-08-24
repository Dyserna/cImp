use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, EventTarget, State};

use crate::error::{AppError, AppResult};
use crate::ipc::windows::{open_or_focus_settings, SETTINGS_LABEL};
use crate::ipc::AppState;
use crate::pty::PtyHost;
use crate::service::on_blocking_pool as run_on_blocking_pool;
use crate::service::pty::PtyService;
use crate::service::checks::{ApplySummary, ChecksService, ChecksSuggestion};
use crate::service::settings::SettingsService;
use crate::service::sink::{OutputSink, TauriEventSink};
use crate::service::workbench::WorkbenchUseCases;
use crate::settings::{AiToolTabConfig, Settings, TabConfig};
use crate::state::{ReadOnlySource, StateSignal, TabId, TabKind};

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
    let registry = state.tabs.lock().await;
    registry.scrollback_snapshot(tab).await
}

#[tauri::command]
pub async fn pty_write(state: State<'_, AppState>, tab: TabId, input: String) -> AppResult<()> {
    // The reserved dashboard tabs are read-only — app-rendered with no PTY
    // of their own — so swallow any write. Defense-in-depth behind the
    // frontend's read-only guard; one shared predicate so a new reserved
    // dashboard can't miss this swallow.
    if tab.is_reserved_dashboard() {
        return Ok(());
    }

    // V39 Phase A (locked decision 4): the read-only refusal. It sits here,
    // ahead of EVERY side effect below — the TTS-marker registration, the
    // typed-input accumulator, the keystroke/submit state signals — because a
    // refused keystroke must leave no trace: an input that never reached the
    // PTY must not have moved the avatar to Listening or armed a TTS-echo
    // suppression for text the model never saw. This is the enforcement point;
    // the xterm widget's own gate is a courtesy that keeps the round trip out
    // of the common case, and is not what makes the lock hold.
    //
    // **Terminal protocol replies are exempt.** xterm answers the *program's*
    // own queries (cursor-position reports, device attributes, focus in/out)
    // on this same channel; they are the terminal talking to the TUI, not the
    // user typing, and refusing them wedges a harness that is waiting for one
    // — precisely while a delegation is driving it. Same predicate the
    // keystroke bookkeeping below uses, so the two can't disagree about what
    // counts as user input.
    if let Some(source) = state.read_only.read_only(&tab) {
        // The driver's *name* (not its id) is what the refusal says, and names
        // live in the tab registry. Looked up only on the `Driven` branch so a
        // plain user lock costs no registry lock.
        let driver_name = match &source {
            ReadOnlySource::User => None,
            ReadOnlySource::Driven { by } => {
                let registry = state.tabs.lock().await;
                registry.name_of(by)
            }
        };
        if let Some(reason) = read_only_refusal(&source, &input, driver_name.as_deref()) {
            tracing::debug!(?tab, %reason, "pty_write refused: tab is read-only");
            return Err(AppError::ReadOnly {
                tab: tab.as_str().to_string(),
                reason,
            });
        }
    }

    // The user's own keystrokes: whether this write submits is read off the
    // bytes, exactly as it always was.
    let submit = Submit::from_input(&input);
    write_through_pipeline(&state, &tab, input, submit).await
}

/// **Does this write submit the turn?** (V39 review L-1.)
///
/// It used to be inferred from the bytes in every case, and for a keyboard that
/// is right — a person's Enter IS the submit. For the delegation engine it is
/// not: the engine's PASTE contains the request's own newlines (a multi-line
/// request is one bracketed paste), so `contains_enter` fired on the paste, one
/// write EARLY. The `UserSubmit` that went out then cleared the worker's prompt
/// mirror and zeroed its input counter for a turn that had not been submitted
/// yet — while the engine's real submit, a lone CR a moment later, is what
/// actually starts it.
///
/// So the caller says. `pty_write` keeps the old inference; the engine passes
/// `No` for the paste and `Yes` for the submit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Submit {
    Yes,
    No,
}

impl Submit {
    /// The keyboard's rule: any CR or LF in the bytes submits.
    pub(crate) fn from_input(input: &str) -> Self {
        if contains_enter(input) {
            Submit::Yes
        } else {
            Submit::No
        }
    }

    fn is_yes(self) -> bool {
        self == Submit::Yes
    }
}

/// **The one input pipeline** (V39 cross-module invariant): every byte that
/// reaches a tab's PTY passes through here — the TTS-marker pre-registration,
/// the typed-input accumulator, the keystroke/submit state signals, and the
/// registry write, in that order and under one registry lock.
///
/// Split out of [`pty_write`] in V39 Phase B so the delegation engine can reuse
/// it. What the engine skips is **only** the read-only check above, and only
/// because that check is about the *user's keyboard*: the engine holds the
/// `Driven` lock itself, so entering through `pty_write` would have it refuse
/// its own write. Everything else it must not skip — a delegated turn that
/// bypassed the TTS-marker registration would have the worker's echo of the
/// task spoken aloud, and one that bypassed `UserSubmit` would leave the
/// avatar in the wrong state for a turn that really did start.
///
/// Takes `&AppState` rather than `State<'_, AppState>` so both a Tauri command
/// and a plain async caller reach the same body.
pub(crate) async fn write_through_pipeline(
    state: &AppState,
    tab: &TabId,
    input: String,
    // V39 review L-1: whether this write SUBMITS. See [`Submit`] — the engine's
    // paste carries newlines, and inferring it from the bytes fired
    // `UserSubmit` one write early.
    submit: Submit,
) -> AppResult<()> {
    let tab = tab.clone();

    // Take the registry lock once at the top so the keystroke / submit
    // counter updates and the final write run inside the same critical
    // section. Pre-V0.6 the counter Arc was cloned out under the read
    // lock and used after dropping it, racing with `close_tab` which
    // removes the counter. Holding the lock end-to-end eliminates that
    // window: if the tab was just closed, `registry.write` errors out
    // cleanly with `unknown tab` and no half-applied state remains.
    let registry = state.tabs.lock().await;

    let existing = {
        // The counter is only the idle-Listening heuristic; a poisoned lock
        // must NOT gate input delivery, or a prior panic would silently drop
        // all keystrokes for the rest of the session. Recover the inner value
        // (matches the poison-recovery pattern used in `tts_stop`/`mutate`).
        let map = state
            .input_lengths
            .read()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&tab).cloned()
    };
    let len_counter = match existing {
        Some(c) => c,
        None => {
            // The state manager may not have drained `TabAdded` yet — there's
            // no happens-before between `create_*_tab` returning and the
            // manager inserting this tab's counter. A missing counter must NOT
            // gate input delivery (it's only the idle-Listening heuristic), or
            // the very first keystrokes into a just-created tab are silently
            // dropped. Lazily insert one; the manager's own insert uses
            // `or_insert_with`, so it won't clobber this.
            let mut map = state
                .input_lengths
                .write()
                .unwrap_or_else(|e| e.into_inner());
            map.entry(tab.clone())
                .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)))
                .clone()
        }
    };

    if !is_automatic_terminal_response(&input) {
        if submit.is_yes() {
            len_counter.store(0, Ordering::Relaxed);
            let _ = state
                .state_signals
                .try_send(StateSignal::UserSubmit { tab: tab.clone() });
        } else {
            apply_input_delta(&input, &len_counter);
            let _ = state
                .state_signals
                .try_send(StateSignal::UserKeystroke { tab: tab.clone() });
            // Note: typing does NOT interrupt TTS. By design, in-flight
            // speech is only stopped by Esc (`tts_stop`) or by switching
            // tabs (so the previous tab's audio doesn't bleed into the new
            // view). Keystrokes still drive avatar state via UserKeystroke.
        }
    }

    registry.write(tab, input.into_bytes()).await
}

/// V39 Phase A: the read-only decision for one write, as a value.
///
/// `Some(reason)` refuses; `None` lets the write through. Two rules, and the
/// second is the one worth pinning in a test:
///
/// 1. A locked tab refuses the user's input, naming the source.
/// 2. **Terminal protocol replies always pass.** xterm answers the running
///    program's own queries — cursor-position reports, device attributes,
///    focus in/out — over the same channel as keystrokes. Those are the
///    terminal talking to the TUI, not a person typing; a harness that asked
///    for the cursor position and never gets an answer can wedge, and it would
///    wedge exactly while a delegation is driving it. The same predicate the
///    keystroke bookkeeping uses decides this, so the two cannot disagree
///    about what counts as user input.
fn read_only_refusal(
    source: &ReadOnlySource,
    input: &str,
    driver_name: Option<&str>,
) -> Option<String> {
    if read_only_exempt(input) {
        return None;
    }
    Some(source.reason(driver_name))
}

/// Everything the read-only lock lets through. **Only** `read_only_refusal`
/// asks this — `is_automatic_terminal_response` keeps its own, unchanged
/// meaning for the keystroke/submit bookkeeping, which must go on treating a
/// wheel report as the non-typing event it always was.
///
/// Two exemptions, for two different reasons:
///
/// 1. The terminal answering the running program (see `read_only_refusal`).
/// 2. **Wheel reports: scrolling is reading.** A read-only tab exists so the
///    user can watch it, and in an alt-screen TUI the wheel is not local
///    scrollback — it is forwarded to the program as a mouse report, so a
///    swallowed wheel means a tab the user is allowed to watch but not scroll.
///    Mouse *clicks* stay refused: a click activates a control (choosing a
///    permission option, for one), which is exactly the input the lock is for.
fn read_only_exempt(input: &str) -> bool {
    is_automatic_terminal_response(input) || is_mouse_wheel(input)
}

/// Whether `input` is *nothing but* mouse-wheel reports.
///
/// Whole-input, and repeat-until-exhausted rather than "starts with": a chunk
/// that is a wheel report followed by typed text is refused, so the exemption
/// cannot be used to smuggle a keystroke past the lock. Repeats are allowed
/// because a fast scroll can arrive as several reports in one chunk, and
/// letting only the first through would drop the rest silently.
fn is_mouse_wheel(input: &str) -> bool {
    let mut rest = input;
    let mut seen = false;
    while !rest.is_empty() {
        match take_wheel_report(rest) {
            Some(next) => {
                seen = true;
                rest = next;
            }
            None => return false,
        }
    }
    seen
}

/// Consume one leading wheel report, returning what follows it.
///
/// Handles both encodings xterm can emit: SGR (`ESC [ < Cb ; Cx ; Cy M`) and
/// the legacy X10/normal one (`ESC [ M` + three bytes, each offset by 32 —
/// read as `char`s so xterm's UTF-8 extended coordinates don't split).
///
/// SGR wheel reports end in `M` only; xterm emits no release for a wheel, so a
/// `…m` form is not recognized and is refused like any other click release.
fn take_wheel_report(s: &str) -> Option<&str> {
    if let Some(body) = s.strip_prefix("\x1b[<") {
        let end = body.find('M')?;
        let (params, after) = body.split_at(end);
        let rest = &after['M'.len_utf8()..];
        let mut parts = params.split(';');
        let cb: u32 = parts.next()?.parse().ok()?;
        let _x: u32 = parts.next()?.parse().ok()?;
        let _y: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        return is_wheel_button(cb).then_some(rest);
    }
    if let Some(body) = s.strip_prefix("\x1b[M") {
        let mut chars = body.chars();
        let cb = (chars.next()? as u32).checked_sub(32)?;
        let _x = chars.next()?;
        let _y = chars.next()?;
        return is_wheel_button(cb).then_some(chars.as_str());
    }
    None
}

/// The wheel bit is 64 (buttons 64/65 vertical, 66/67 horizontal). Bit 32 is
/// motion, which a wheel never sets and a drag always does, so it must be
/// clear. Modifier bits (shift 4, meta 8, ctrl 16) may be set — ctrl+wheel is
/// still a wheel. Nothing at or above 128 is a mouse button.
fn is_wheel_button(cb: u32) -> bool {
    cb < 128 && (cb & 0b110_0000) == 0b100_0000
}

/// V39 Phase A: set or clear a tab's **user** read-only lock (locked
/// decision 4's `ReadOnlySource::User`) — the Access radio in the tab's
/// communication popover.
///
/// Does two things, in this order: takes the runtime lock (so it is in force
/// before this call returns, with no window in which the UI shows "read-only"
/// and the PTY still accepts keys), then persists the flag so it survives a
/// restart. The persisted write broadcasts `settings-changed`, which is how
/// the frontend learns the new state — there is no separate event.
///
/// Only ever sets `User`. The engine's `Driven` lock is not reachable from
/// here: it belongs to a delegation's lifetime, and "Take over" (Phase B), not
/// a radio button, is what ends one.
#[tauri::command]
pub async fn tab_set_read_only(state: State<'_, AppState>, tab: TabId, on: bool) -> AppResult<()> {
    if tab.kind() != TabKind::AiTool {
        return Err(AppError::Ipc(format!(
            "tab `{}` is not an AI tab; the read-only lock applies to AI tabs only",
            tab.as_str()
        )));
    }
    if !matches!(
        state.settings.current().find_tab(tab.as_str()),
        Some(TabConfig::AiTool(_))
    ) {
        return Err(AppError::Ipc(format!(
            "unknown AI tab `{}`",
            tab.as_str()
        )));
    }
    state.read_only.set_user(&tab, on);
    let id = tab.as_str().to_string();
    state.settings.mutate(move |snap| {
        if let Some(TabConfig::AiTool(cfg)) = snap.find_tab_mut(&id) {
            cfg.read_only = on;
        }
    });
    Ok(())
}

/// V39 Phase B: what `tab_set_delegation_role` did, for the UI's toast.
///
/// `displaced` is the id of the tab that LOST the Manual role to this call
/// (locked decision 8's move rule) — `None` when nothing moved. Returned rather
/// than only recorded because the losing tab may not be visible, and a role
/// that moved silently is a `delegate_task_*` tool that started driving a
/// different tab with nothing on screen saying so.
#[derive(serde::Serialize)]
pub struct RoleChange {
    pub tab: String,
    pub role: crate::settings::DelegationRole,
    pub displaced: Option<String>,
}

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
    use crate::settings::DelegationRole;

    if tab.is_reserved_dashboard() {
        return Err(AppError::Ipc(format!(
            "tab `{}` is an app-rendered dashboard, not a harness tab; it has no delegation role",
            tab.as_str()
        )));
    }
    if tab.kind() != TabKind::AiTool {
        return Err(AppError::Ipc(format!(
            "tab `{}` is not an AI tab; delegation roles apply to AI tabs only",
            tab.as_str()
        )));
    }
    let settings = state.settings.current();
    let Some(TabConfig::AiTool(cfg)) = settings.find_tab(tab.as_str()) else {
        return Err(AppError::Ipc(format!("unknown AI tab `{}`", tab.as_str())));
    };
    let Some(agent) = crate::tabs::tab_consumer(cfg) else {
        // V40 Phase A (locked decision 2): a tab whose command names no
        // registered harness is not a worker at all. It used to be classified
        // as OpenCode here, become eligible for that harness's Manual slot, and
        // be typed into with OpenCode's paste rules.
        return Err(AppError::Ipc(format!(
            "tab `{}` runs no registered harness, so cImp has no way to type a turn into \
             it - it cannot hold a delegation role",
            tab.as_str()
        )));
    };
    if crate::harness::input_profile(agent).is_none() {
        return Err(AppError::Ipc(format!(
            "tab `{}` runs a harness with no input profile, so cImp could never type a turn into \
             it — it cannot hold a delegation role",
            tab.as_str()
        )));
    }

    // Who currently holds Manual for this harness, if anyone. Read before the
    // mutation so the row and the return value name the same tab the mutation
    // is about to clear.
    let displaced: Option<(String, String)> = if role == DelegationRole::Manual {
        settings.tabs.iter().find_map(|t| match t {
            TabConfig::AiTool(c)
                if c.delegation_role == DelegationRole::Manual
                    && c.id != tab.as_str()
                    && crate::tabs::tab_consumer(c) == Some(agent) =>
            {
                Some((c.id.clone(), c.name.clone()))
            }
            _ => None,
        })
    } else {
        None
    };

    let id = tab.as_str().to_string();
    let losing = displaced.as_ref().map(|(id, _)| id.clone());
    let agent_for_mutate = agent;
    state.settings.mutate(move |snap| {
        // ONE mutation for both writes: a snapshot in which two tabs of one
        // harness hold Manual must never be observable by a broadcast reader.
        for t in snap.tabs.iter_mut() {
            let TabConfig::AiTool(c) = t else { continue };
            if c.id == id {
                c.delegation_role = role;
            } else if role == DelegationRole::Manual
                && c.delegation_role == DelegationRole::Manual
                && crate::tabs::tab_consumer(c) == Some(agent_for_mutate)
            {
                c.delegation_role = DelegationRole::None;
            }
        }
    });

    if let Some((lost_id, lost_name)) = &displaced {
        let taker = {
            let registry = state.tabs.lock().await;
            registry
                .name_of(&tab)
                .unwrap_or_else(|| tab.as_str().to_string())
        };
        crate::delegation::record_row(
            crate::delegation::transition::ROLE_MOVED,
            lost_name,
            Some(&format!(
                "the Manual role for this harness moved to `{taker}`"
            )),
            agent,
            Some(tab.as_str()),
            true,
            0,
            String::new(),
            String::new(),
        );
        tracing::info!(from = %lost_id, to = %tab.as_str(), harness = %agent, "delegation: Manual role moved");
    }

    Ok(RoleChange {
        tab: tab.as_str().to_string(),
        role,
        displaced: losing,
    })
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
    if tab.kind() != TabKind::AiTool {
        return Err(AppError::Ipc(format!(
            "tab `{}` is not an AI tab; delegation backends are configured on AI tabs only",
            tab.as_str()
        )));
    }
    if !matches!(
        state.settings.current().find_tab(tab.as_str()),
        Some(TabConfig::AiTool(_))
    ) {
        return Err(AppError::Ipc(format!("unknown AI tab `{}`", tab.as_str())));
    }
    let backend = normalise_backend(backend);
    let id = tab.as_str().to_string();
    state.settings.mutate(move |snap| {
        if let Some(TabConfig::AiTool(cfg)) = snap.find_tab_mut(&id) {
            apply_backend_patch(cfg, backend);
        }
    });
    Ok(())
}

/// The two "blank means unset" rules, at the parse boundary rather than at
/// every reader: a cleared text field arrives as `""` and a cleared number
/// field as `0`, and both mean "use the default".
fn normalise_backend(
    mut backend: crate::settings::DelegationBackend,
) -> crate::settings::DelegationBackend {
    backend.name = backend
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    backend.declared_context = backend.declared_context.filter(|n| *n > 0);
    backend
}

/// Write the knobs onto one tab's config. **Only** the knobs — separated out
/// so a test can state that, since the command itself needs a running app.
fn apply_backend_patch(
    cfg: &mut crate::settings::AiToolTabConfig,
    backend: crate::settings::DelegationBackend,
) {
    cfg.delegation_backend = backend;
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
#[tauri::command]
pub async fn delegation_status(tab: TabId) -> AppResult<Option<crate::delegation::InFlightView>> {
    Ok(crate::delegation::status(&tab))
}

/// V39 Phase B: every in-flight delegation, keyed by worker tab id — the
/// status-bar chip's count and the initial paint of every tab's glyph, in one
/// call rather than one per tab.
#[tauri::command]
pub async fn delegation_statuses() -> AppResult<Vec<(String, crate::delegation::InFlightView)>> {
    Ok(crate::delegation::statuses())
}

fn contains_enter(input: &str) -> bool {
    input.chars().any(|c| c == '\r' || c == '\n')
}

fn apply_input_delta(input: &str, length: &std::sync::atomic::AtomicI32) {
    if input.starts_with('\x1b') {
        return;
    }
    let mut current = length.load(Ordering::Relaxed);
    for c in input.chars() {
        match c {
            '\x08' | '\x7f' => current = (current - 1).max(0),
            '\x15' | '\x0b' => current = 0,
            '\x17' => current = (current - 4).max(0),
            c if c.is_control() => {}
            _ => current += 1,
        }
    }
    length.store(current, Ordering::Relaxed);
}

fn is_automatic_terminal_response(input: &str) -> bool {
    if input == "\x1b[I" || input == "\x1b[O" {
        return true;
    }
    let bytes = input.as_bytes();
    if bytes.len() < 3 || bytes[0] != 0x1b || bytes[1] != b'[' {
        return false;
    }
    let last = bytes[bytes.len() - 1];
    if !matches!(last, b'R' | b'c' | b'n') {
        return false;
    }
    bytes[2..bytes.len() - 1]
        .iter()
        .all(|&b| b.is_ascii_digit() || b == b';' || b == b'?')
}

/// Debug: synthesize and play `text` directly through the TTS worker, skipping
/// the processor. Routed as if it came from the active tab so the worker's
/// filter doesn't drop it.
#[tauri::command]
pub async fn tts_test(state: State<'_, AppState>, text: String) -> AppResult<()> {
    let active = state.tabs.lock().await.active();
    state
        .tts_segments
        .send(crate::tts::TtsRequest::Synthesize {
            tab: active,
            text,
            suppressible: false,
        })
        .await
        .map_err(|e| AppError::Tts(format!("tts_test send: {e}")))?;
    Ok(())
}

/// Read arbitrary text aloud through the TTS worker, skipping the
/// processor. Backs the Ctrl+right-click "speak selection" gesture
/// (`behavior.speak_selection_on_right_click`). Routed as if it came
/// from the active tab so the worker's background-tab filter doesn't
/// drop it. Whitespace-only text is ignored — the frontend guards too,
/// but a backend skip keeps an empty synthesis off the worker.
#[tauri::command]
pub async fn tts_speak(state: State<'_, AppState>, text: String) -> AppResult<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let active = state.tabs.lock().await.active();
    state
        .tts_segments
        .send(crate::tts::TtsRequest::Synthesize {
            tab: active,
            text,
            suppressible: false,
        })
        .await
        .map_err(|e| AppError::Tts(format!("tts_speak send: {e}")))?;
    Ok(())
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
    if chunks.is_empty() {
        return Ok(());
    }
    // Resolve the active tab FIRST (this awaits the registry lock), then arm
    // the session cell immediately before the send with no await in between.
    // Storing the session before the `.lock().await` left a window in which a
    // concurrent `tts_stop` (Esc) could zero the cell and then this command
    // would still proceed to send — racing the stop. With the store moved
    // after the await, an Esc that lands before this point simply means the
    // worker sees a superseding/zeroed cell and abandons, and one that lands
    // after is a clean supersede. The worker re-checks `speak_session` both
    // before and after each chunk's synthesis, so a stop during the read still
    // cancels the remaining chunks.
    let active = state.tabs.lock().await.active();
    state.speak_session.store(session, Ordering::SeqCst);
    state
        .tts_segments
        .send(crate::tts::TtsRequest::SpeakSelection {
            tab: active,
            session,
            chunks,
        })
        .await
        .map_err(|e| AppError::Tts(format!("tts_speak_selection send: {e}")))?;
    Ok(())
}

/// Stop all TTS playback immediately and cancel any in-flight selection read.
/// Backs the Esc gesture: clears the audio sink (so queued chunks never play)
/// and zeroes the shared session cell (so the worker abandons the remaining
/// chunks it hasn't enqueued yet). The frontend clears its highlight on the
/// same Esc.
#[tauri::command]
pub async fn tts_stop(state: State<'_, AppState>) -> AppResult<()> {
    state.speak_session.store(0, Ordering::SeqCst);
    // Suppress the rest of the current AI-output burst's tagged segments
    // (those still queued or yet to arrive) until the next `HarnessOutputStarted`
    // clears the flag. Notifications and selection reads are unaffected — they
    // ride other request variants the worker doesn't gate on this flag.
    state.ai_tts_suppressed.store(true, Ordering::SeqCst);
    // Recover the guard even if the lock is poisoned: this is the Esc
    // emergency-stop, so it must never silently no-op and leave audio playing
    // with no way to stop it from the UI. `into_inner` hands back the guard;
    // the data behind it (an `Option<AudioOutput>` handle) is not left in a
    // broken state by a panicking writer.
    let audio = state.audio.read().unwrap_or_else(|e| e.into_inner());
    if let Some(audio) = audio.as_ref().cloned() {
        audio.stop_all();
    }
    Ok(())
}

/// Pause or resume TTS playback without discarding queued audio. Backs the
/// bottom-bar selection-TTS pause/resume transport. The in-flight read's
/// session is left untouched (so resume continues exactly where it paused);
/// only the audio sink is paused.
#[tauri::command]
pub async fn tts_set_paused(state: State<'_, AppState>, paused: bool) -> AppResult<()> {
    // Recover a poisoned guard rather than swallowing it — a no-op pause/resume
    // would leave the transport controls dead with no signal why.
    let audio = state.audio.read().unwrap_or_else(|e| e.into_inner());
    if let Some(audio) = audio.as_ref().cloned() {
        audio.set_paused(paused);
    }
    Ok(())
}

// --- Speech-to-text (V6-01) -------------------------------------------------
//
// The handle just posts commands to the capture thread; recording/transcribe
// state transitions and the resulting transcript arrive on the frontend via
// the `stt-state` / `stt-transcription` events, not these return values.

/// Open the input device and begin capturing. No-op if already recording.
#[tauri::command]
pub async fn stt_start_recording(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.start();
    Ok(())
}

/// Stop capturing and hand the recording to the transcription worker. The
/// transcript arrives later via the `stt-transcription` event.
#[tauri::command]
pub async fn stt_stop_recording(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.stop();
    Ok(())
}

/// Stop capturing and discard the buffer (no transcription).
#[tauri::command]
pub async fn stt_cancel(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.cancel();
    Ok(())
}

/// List the `ggml-*.bin` Whisper models present under `models/` for the
/// settings dropdown.
#[tauri::command]
pub async fn stt_list_models() -> AppResult<Vec<String>> {
    crate::stt::list_models()
}

/// List cpal input device names for the settings device picker. The frontend
/// prepends a "System default" entry (which maps to an empty `input_device`).
#[tauri::command]
pub async fn stt_list_input_devices() -> AppResult<Vec<String>> {
    crate::stt::list_input_devices()
}

/// **One harness's usage reading** for the bottom-bar tracker (V40 Phase D,
/// locked decision 19). Local read, never the network.
///
/// This was `get_claude_usage`, a command named after a harness that answered
/// a payload with `five_hour` / `seven_day` fields in it. It now takes the
/// harness and answers three distinguishable states, which is the whole point:
///
/// * `source: None` — **this harness has no usage source at all.** OpenCode.
///   A UI must render that as absence; rendering it as a harness at 0% would be
///   a number nobody reported (global principle 5). It says nothing about
///   whether the harness RECORDS turns — see `token_kinds`/`origins` below.
/// * `source: Some(..), reading: None` — it has one, and nothing has been
///   reported yet (no tab of that harness has pushed, or the last push aged
///   out).
/// * `source: Some(..), reading: Some(..)` — the declared windows that have a
///   reading, in declared order, plus the live context block.
///
/// Beside those three, and **independent of them**, the answer carries the
/// declared shape of a RECORDED turn: `token_kinds` and `origins`. V40 Phase G
/// split the two questions, because they had different answers all along —
/// OpenCode reports no quota and no context window (`source: None`) and still
/// writes real per-turn token rows with a parent/child lane split. Nesting the
/// declaration under `source` meant the Usage donut could not label an OpenCode
/// session's lanes at all. Both lists are EMPTY for a harness that declares no
/// turn shape.
///
/// An unregistered harness id is an error, not an empty reading: a widget
/// polling for a harness that does not exist is a bug, and answering it with
/// "nothing to show" would hide it forever.
#[tauri::command]
pub async fn harness_usage(harness: String) -> AppResult<HarnessUsage> {
    let id = crate::harness::HarnessId::from_id(&harness).ok_or_else(|| {
        crate::error::AppError::Ipc(format!(
            "unknown harness `{harness}` — registered: {}",
            crate::harness::registry::harness_ids().join(", ")
        ))
    })?;
    let plugin = id.plugin();
    let source = plugin.and_then(|p| p.usage_source());
    let shape = plugin.and_then(|p| p.turn_usage_shape());
    Ok(HarnessUsage {
        source: source.map(|s| UsageSourceInfo {
            windows: s
                .windows()
                .iter()
                .map(|w| DeclaredWindow {
                    id: w.id,
                    label: w.label,
                    short: w.short,
                    description: w.description,
                })
                .collect(),
        }),
        token_kinds: shape
            .map(|s| {
                s.token_kinds
                    .iter()
                    .map(|k| DeclaredLabel { id: k.id, label: k.label })
                    .collect()
            })
            .unwrap_or_default(),
        origins: shape
            .map(|s| {
                s.origins
                    .iter()
                    .map(|o| DeclaredOrigin { id: o.id, label: o.label, subagent: o.subagent })
                    .collect()
            })
            .unwrap_or_default(),
        reading: source.and_then(|s| s.read()),
    })
}

/// The answer [`harness_usage`] gives. See its docs for the three source
/// states and for why the turn shape sits BESIDE them, not inside them.
#[derive(serde::Serialize)]
pub struct HarnessUsage {
    /// What this harness's quota source *can* report — `None` when it has none.
    pub source: Option<UsageSourceInfo>,
    /// The billing categories this harness reports a RECORDED turn's tokens
    /// under, in declared order. Empty when it records no turns.
    pub token_kinds: Vec<DeclaredLabel>,
    /// The lanes a recorded turn can be attributed to, in declared order —
    /// what the Usage donut labels its rings with. Empty when it records no
    /// turns.
    pub origins: Vec<DeclaredOrigin>,
    /// What the quota source reports right now.
    pub reading: Option<crate::harness::plugin::UsageReading>,
}

/// The declared shape of a QUOTA source: which windows it can report.
///
/// Sent alongside the reading rather than mirrored in the frontend, so a
/// harness with three quota windows (or one, or none) needs no UI change —
/// locked decision 19. V40 Phase G moved `token_kinds` / `origins` OUT of here:
/// they describe a stored turn, not a quota reading, and a harness can have
/// either without the other.
#[derive(serde::Serialize)]
pub struct UsageSourceInfo {
    pub windows: Vec<DeclaredWindow>,
}

/// One declared quota window, without a reading.
#[derive(serde::Serialize)]
pub struct DeclaredWindow {
    pub id: &'static str,
    pub label: &'static str,
    pub short: &'static str,
    pub description: &'static str,
}

/// A declared id with the label a UI renders for it (token categories).
#[derive(serde::Serialize)]
pub struct DeclaredLabel {
    pub id: &'static str,
    pub label: &'static str,
}

/// One declared turn lane. Carries `subagent` because that is what tells a UI
/// which lane gets the fan-out treatment (the outlined bar, the "A" badge)
/// without recognising the word `"agent"` — the literal locked decision 19
/// exists to delete.
#[derive(serde::Serialize)]
pub struct DeclaredOrigin {
    pub id: &'static str,
    pub label: &'static str,
    pub subagent: bool,
}

/// Sample the system-monitor stats (CPU / memory / GPU / network) for the
/// bottom-bar panel. Polled by the frontend on `system_stats.poll_interval_secs`
/// (default 1s); the frontend keeps its own history for the sparklines.
#[tauri::command]
pub async fn get_system_stats(
    state: State<'_, AppState>,
) -> AppResult<crate::sysmon::SystemStatsSnapshot> {
    // `sample()` blocks: it does a synchronous sysinfo refresh (incl.
    // `networks.refresh(true)`, which re-scans every interface) plus NVML
    // device queries. Run it on the blocking pool so the 1 Hz poll doesn't
    // stall other IPC futures on the async reactor thread.
    let sysmon = state.sysmon.clone();
    tauri::async_runtime::spawn_blocking(move || sysmon.sample())
        .await
        .map_err(|e| AppError::Ipc(format!("system stats join: {e}")))
}

#[tauri::command]
pub async fn pty_resize(
    state: State<'_, AppState>,
    tab: TabId,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    let registry = state.tabs.lock().await;
    registry.resize(tab, rows, cols).await
}

#[tauri::command]
pub async fn compose_content_changed(state: State<'_, AppState>, non_empty: bool) -> AppResult<()> {
    // Compose targets the currently active tab — its non-empty edge promotes
    // the active tab Idle→Listening and pins Listening while content remains.
    let active = state.tabs.lock().await.active();
    let _ = state
        .state_signals
        .try_send(StateSignal::ComposeContentChanged {
            tab: active,
            non_empty,
        });
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
#[tauri::command]
pub async fn compose_attach_image(state: State<'_, AppState>, bytes: Vec<u8>) -> AppResult<String> {
    let session = state.launch.launch_id.clone();
    let path = crate::attach::save_png(&session, &bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn acknowledge_error(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    let _ = state
        .state_signals
        .try_send(StateSignal::ErrorAcknowledged { tab });
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
    let registry = state.tabs.lock().await;
    Ok(registry.list())
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

/// Current offload server status (state + discovered `n_ctx`/slots/in-flight)
/// for the **primary** local backend (legacy single-status readout).
#[tauri::command]
pub async fn offload_status(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<crate::offload::OffloadState> {
    Ok(supervisor.status().await)
}

/// V8-02: per-backend status for every enabled backend in the pool (Local
/// process+health and Remote health-probe). Drives the Settings backends
/// editor's status rows.
#[tauri::command]
pub async fn offload_statuses(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<Vec<crate::offload::supervisor::BackendStatus>> {
    Ok(supervisor.statuses().await)
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
    supervisor
        .inner()
        .start_backend(
            &name,
            command_override,
            crate::offload::supervisor::StartCause::Ipc,
        )
        .await
}

/// V8-02: stop one named Local backend (idempotent).
#[tauri::command]
pub async fn offload_backend_stop(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    supervisor
        .stop_backend(&name, crate::offload::supervisor::StopCause::Ipc)
        .await;
    Ok(())
}

/// V8-02: restart (Reset) one named Local backend.
#[tauri::command]
pub async fn offload_backend_restart(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    supervisor.inner().restart_backend(&name).await
}

/// Start the offload `llama-server` (idempotent).
#[tauri::command]
pub async fn offload_server_start(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    supervisor
        .inner()
        .start(crate::offload::supervisor::StartCause::Ipc)
        .await
}

/// Stop the offload `llama-server` (idempotent).
#[tauri::command]
pub async fn offload_server_stop(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    supervisor
        .stop(crate::offload::supervisor::StopCause::Ipc)
        .await;
    Ok(())
}

/// Reset: kill + respawn with the current `server_command` (re-health,
/// re-read `n_ctx`/`np`).
#[tauri::command]
pub async fn offload_server_restart(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    supervisor.inner().restart().await
}

/// Run a canned offload task against the local server and return its
/// answer (the Settings "Test offload" button).
#[tauri::command]
pub async fn offload_test(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    instructions: String,
) -> AppResult<String> {
    let task = if instructions.trim().is_empty() {
        "Briefly confirm you are reachable and list the tools available to you.".to_string()
    } else {
        instructions
    };
    supervisor
        .inner()
        .run_task(task, crate::offload::agent::ThinkingMode::Auto)
        .await
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
    let writers: Vec<crate::harness::HarnessId> = match harness.as_deref().map(str::trim) {
        Some(h) if !h.is_empty() => vec![crate::harness::HarnessId::from_id(h).ok_or_else(|| {
            crate::error::AppError::Offload(format!("{h:?} names no registered harness"))
        })?],
        _ => crate::harness::registry::all()
            .filter(|h| h.plugin().is_some_and(|p| p.config_writer().is_some()))
            .collect(),
    };
    let [only] = writers[..] else {
        return Err(crate::error::AppError::Offload(format!(
            "which harness should this provider be written for? {} of them accept one — name it",
            writers.len()
        )));
    };
    let writer = only
        .plugin()
        .and_then(|p| p.config_writer())
        .ok_or_else(|| {
            crate::error::AppError::Offload(format!(
                "{} is not configured through a provider block cImp writes",
                only.label()
            ))
        })?;
    writer.derive_local_provider(&server_command)
}

/// V8-03: aggregate offload-service status — the honest global in-flight
/// count (now that the long-lived app sees every offload) and per-MCP-server
/// health rows. Drives the Settings warm-pool readout.
#[tauri::command]
pub async fn offload_service_status(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<crate::offload::service::ServiceStatus> {
    Ok(service.status().await)
}

/// Reconcile the warm MCP host against the *current* settings and return the
/// fresh status. The Settings MCP editor calls this right after persisting an
/// add/remove/enable/disable so a server connects or drops live — no app
/// restart. Cheap when the pool is already warm (unchanged servers are kept).
#[tauri::command]
pub async fn offload_reload_mcp(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<crate::offload::service::ServiceStatus> {
    service.warm_host().await;
    Ok(service.status().await)
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
    let settings = state.settings.current();
    let status = tokio::task::spawn_blocking(move || {
        if reload {
            crate::offload::detection::reload(&settings)
        } else {
            crate::offload::detection::status(&settings)
        }
    })
    .await
    .map_err(|e| AppError::Offload(format!("detection status task failed: {e}")))?;
    Ok(status)
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
    use crate::offload::detection::updater::{self, manifest::Component};
    let settings = state.settings.current();
    updates_allowed(&settings)?;
    let components: Vec<Component> = match component.as_deref() {
        None | Some("") => Component::ALL.to_vec(),
        Some(name) => vec![Component::parse(name).ok_or_else(|| {
            AppError::Offload(format!(
                "unknown detection component `{name}` (expected \"rules\")"
            ))
        })?],
    };
    updater::run_live(&components, &settings, apply).await;
    Ok(crate::offload::detection::status(&settings))
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
    use crate::offload::detection::updater::{self, manifest::Component};
    let settings = state.settings.current();
    updates_allowed(&settings)?;
    let c = Component::parse(&component).ok_or_else(|| {
        AppError::Offload(format!(
            "unknown detection component `{component}` (expected \"rules\" or \"classifier\")"
        ))
    })?;
    tokio::task::spawn_blocking(move || updater::revert_live(c))
        .await
        .map_err(|e| AppError::Offload(format!("detection revert task failed: {e}")))?;
    Ok(crate::offload::detection::status(&settings))
}

/// The updater's gate for the two manual commands above, resolved through the
/// same [`updater::updates_enabled`](crate::offload::detection::updater::updates_enabled)
/// the scheduler tick uses — one predicate, so a button and a tick can never
/// disagree about whether the feature is on.
///
/// An `Err` rather than a silently unchanged status: a security control that
/// does nothing when clicked, and says nothing about it, teaches the user to
/// distrust it (the same reasoning as `latch_override`'s verbatim errors).
///
/// **#48 (M-21): three refusals, because there are three states and they are
/// different statements.** The gate is unchanged — one predicate,
/// `updates_enabled`, so a button and a tick can still never disagree — but *why*
/// it said no is not always "detection is off". A worker-scope override leaves
/// this updater inert while injection detection is armed for the offload worker,
/// which keeps screening with the bundle already on disk; telling that user their
/// detection is switched off is a false claim about a running security layer, and
/// it is the claim they would act on.
///
/// The third case is M-21's residual, folded in with F-35: the **L1 master** is
/// off, which resolves detection off with it. Saying "injection detection is
/// switched off" there points the user at the wrong switch — the one they can
/// flip without effect until the master above it is back on. `SettingsApp.svelte`
/// had already added this distinction as a frontend refinement; the two surfaces
/// now single-source from the same three cases rather than the tooltip being
/// more specific than the error.
///
/// Checked in the frontend's order, which is also the only correct one: the
/// master-off case cannot collide with `worker_only_detection` (`decide`
/// short-circuits every feature to `false` with L1 off, so no scope is armed),
/// and the generic sentence keeps its parenthetical about the master because it
/// is still the fall-through for a state nobody positively identified.
///
/// **Reporting only, and asserted as such** — every branch still returns `Err`.
/// Reporting honesty must not become a new capability.
fn updates_allowed(settings: &crate::settings::Settings) -> AppResult<()> {
    use crate::offload::detection::updater;
    if updater::updates_enabled(settings) {
        return Ok(());
    }
    if updater::worker_only_detection(settings) {
        return Err(AppError::Settings(
            "injection detection is switched off app-wide and for every AI tab, so the detection \
             updater will not check, apply or revert anything. It is still switched ON for the \
             offload worker, which keeps screening with the rule bundle already on disk — the \
             updater follows the app-wide answer, and one worker override does not start it. To \
             keep that bundle current, turn injection detection back on app-wide in \
             Settings → Injection protection."
                .to_string(),
        ));
    }
    if !crate::settings::injection::master_enabled(settings) {
        return Err(AppError::Settings(
            "injection protection is switched off at the master switch, which resolves injection \
             detection off with it — so the detection updater will not check, apply or revert \
             anything. Turn the master switch, and injection detection under it, back on in \
             Settings → Injection protection."
                .to_string(),
        ));
    }
    // Reached only when the worker's row is off too and the master is on, which
    // is what makes this sentence true rather than merely conventional.
    Err(AppError::Settings(
        "injection detection is switched off, so the detection updater will not check, apply or \
         revert anything. Turn it (and the injection-protection master above it) back on in \
         Settings → Injection protection."
            .to_string(),
    ))
}

/// V32 Phase C3: open `<exe-dir>/detection/rules.d/` in the host file manager,
/// creating it first so the call does not fail on a layout where the folder was
/// never staged. Same shape as [`content_open_folder`] — one pattern for "show
/// me this directory".
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
    Ok(supervisor.server_logs(name))
}

/// V8-03: latest Offload Server dashboard snapshot — one row per enabled
/// backend (Local + Remote), each with slots, throughput, queue, context, and
/// request history. The initial fill for the dashboard; live updates arrive
/// via the `offload-server-metrics` event. Empty before the first poll.
#[tauri::command]
pub async fn offload_server_metrics(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<Vec<crate::offload::metrics::BackendDashboard>> {
    Ok(service.server_metrics())
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
/// path when non-blank, else the app's launch directory. Shared by the graph
/// commands so the fallback lives in one place.
/// Resolve an optional `root` IPC argument to a project directory: the given
/// path when non-blank, else the app's launch directory. Shared by the graph
/// commands so the fallback lives in one place — the service layer needs the
/// same answer, so the rule itself is [`crate::service::project_root`] and this
/// is its name at the wire boundary.
fn resolve_graph_root(root: Option<String>) -> AppResult<std::path::PathBuf> {
    crate::service::project_root(root)
}

/// V9-01: known per-root code-graph status (idle/building/ready/error + row
/// counts). The initial fill for the graph status surface; live transitions
/// arrive via the `graph-status` event. Empty before the first build.
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
#[tauri::command]
pub async fn graph_rebuild(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<()> {
    let root = match root {
        Some(r) if !r.trim().is_empty() => std::path::PathBuf::from(r),
        _ => std::env::current_dir().map_err(|e| AppError::Settings(format!("cwd: {e}")))?,
    };
    // A user clicked Rebuild — the one graph path allowed to announce itself on
    // the V30 session-push bus (and only if it also runs long enough to matter).
    service.spawn_rebuild(root, crate::graph::RebuildOrigin::User);
    Ok(())
}

/// Open a native file/folder picker for the Settings "Ignore" editor and
/// return a gitignore-style glob for the selection: project-relative and
/// anchored with a leading `/` when the pick lies under a known graph root
/// (longest root wins), with a trailing `/` for folders. `None` when the user
/// cancels. A pick outside every root falls back to the absolute path
/// (forward slashes) — it won't match anything, but it lands visibly in the
/// editor where the user can correct it, rather than being silently dropped.
#[tauri::command]
pub async fn graph_ignore_pick(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    folder: bool,
) -> AppResult<Option<String>> {
    let start = std::env::current_dir().ok();
    // rfd's sync dialog blocks its thread (native message pump) — keep it off
    // the async runtime's core threads.
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut d = rfd::FileDialog::new().set_title(if folder {
            "Choose a folder for the graph to ignore"
        } else {
            "Choose a file for the graph to ignore"
        });
        if let Some(s) = start {
            d = d.set_directory(s);
        }
        if folder {
            d.pick_folder()
        } else {
            d.pick_file()
        }
    })
    .await
    .map_err(|e| AppError::Settings(format!("picker task: {e}")))?;
    let Some(path) = picked else { return Ok(None) };

    let mut roots: Vec<std::path::PathBuf> = service
        .statuses()
        .iter()
        .map(|s| std::path::PathBuf::from(&s.root))
        .collect();
    // The launch dir is the primary project even before its first build.
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    Ok(Some(to_ignore_glob(&path, folder, &roots)))
}

/// Turn a picked absolute path into the `graph.ignore` glob `graph_ignore_pick`
/// returns (see its doc for the shape). Split out for testability.
fn to_ignore_glob(path: &std::path::Path, is_dir: bool, roots: &[std::path::PathBuf]) -> String {
    // Longest matching root wins so a nested root maps to the shorter rel.
    let rel = roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())
        .and_then(|r| path.strip_prefix(r).ok());
    let mut glob = match rel {
        // Leading `/` anchors to the project root: the user picked THIS
        // `docs/`, not every directory named `docs` at any depth.
        Some(rel) => format!("/{}", rel.to_string_lossy().replace('\\', "/")),
        None => path.to_string_lossy().replace('\\', "/"),
    };
    if is_dir && !glob.ends_with('/') {
        glob.push('/');
    }
    glob
}

/// V9-01 Phase G: force a full re-embed of the project's doc chunks (drops the
/// vector store, then backfills). The "Rebuild embeddings" action; no-op when
/// semantic search is off. `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_rebuild_embeddings(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    service.spawn_rebuild_embeddings(root);
    Ok(())
}

/// V9-01: probe the configured embedding endpoint on demand (the monitor tab's
/// "Test connection" action). Returns reachability + the live vector dimension
/// or the exact connection error, without running a full embed backfill.
#[tauri::command]
pub async fn graph_test_embedder(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
) -> AppResult<crate::graph::EmbedderProbe> {
    Ok(service.test_embedder().await)
}

/// V9-01: recent graph tool calls (cloud Claude + offload worker), newest
/// first. The store is process-wide across every indexed root; pass
/// `scoped: true` (with an optional `root`, default the launch directory) to
/// filter to one project's calls — the Graph View pulse feed uses this so
/// another project's activity can't light up same-named nodes here.
///
/// The persistent activity store also holds offload runs; this endpoint keeps
/// its historical contract of graph calls only (the pulse feed maps
/// tool/target onto graph nodes). The Tool Activity tab uses
/// [`activity_list`] and sees everything.
/// `since_ts` (optional) trims the response to entries newer than the
/// caller's high-water mark, so the 1.5–2s pollers aren't re-serializing
/// hundreds of unchanged rows every tick. All store calls run on the
/// blocking pool: the first access loads the JSONL mirror from disk, and
/// mutations rewrite it — neither belongs on a tokio worker thread.
#[tauri::command]
pub async fn graph_history(
    root: Option<String>,
    scoped: Option<bool>,
    since_ts: Option<u64>,
) -> AppResult<Vec<crate::activity::ActivityEntry>> {
    let key = if scoped.unwrap_or(false) {
        Some(crate::activity::root_key(&resolve_graph_root(root)?))
    } else {
        None
    };
    run_on_blocking_pool(move || {
        let mut calls: Vec<_> = crate::activity::snapshot_since(since_ts.unwrap_or(0))
            .into_iter()
            .filter(|c| c.kind == crate::activity::ActivityKind::Graph.as_str())
            .collect();
        if let Some(key) = key {
            // #104 item 5: NOT `==`. The store holds rows this project wrote
            // before the key spelling was unified, so a raw compare drops half
            // of one project's history.
            calls.retain(|c| crate::activity::root_key_eq(&c.root, &key));
        }
        calls
    })
    .await
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
    run_on_blocking_pool(move || {
        crate::activity::delete(id);
    })
    .await
}

/// Clear the whole activity history (persists immediately).
#[tauri::command]
pub async fn activity_clear() -> AppResult<()> {
    run_on_blocking_pool(crate::activity::clear).await
}

/// V10: one candidate dead export (unused public symbol) for the Analyses tab.
#[derive(serde::Serialize)]
pub struct DeadExportRow {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub signature: String,
}

/// V10 (Analyses): candidate unused public symbols — public/exported defs with
/// no reference and no inbound call edge. Candidates only; the UI states the
/// false-positive caveat (dynamic dispatch, external API, macros/reflection).
/// `root` defaults to the launch directory. On-demand (no background schedule).
#[tauri::command]
pub async fn graph_dead_exports(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<DeadExportRow>> {
    let root = resolve_graph_root(root)?;
    let hits = service.dead_exports(&root)?;
    Ok(hits
        .into_iter()
        .map(|s| DeadExportRow {
            name: s.name,
            kind: s.kind,
            file: s.file,
            line: s.start_line,
            signature: s.signature,
        })
        .collect())
}

/// V10 (Analyses): import cycles between files (each a loop of ≥ 2 files that
/// transitively import one another). `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_cycles(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<Vec<String>>> {
    let root = resolve_graph_root(root)?;
    service.import_cycles(&root)
}

/// V12 Phase B (Analyses): one symbol changed since `HEAD` (the working-tree
/// diff's root set).
#[derive(serde::Serialize)]
pub struct ChangedSymbolRow {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
}

/// V12 Phase B (Analyses): one transitive dependent of a changed symbol.
#[derive(serde::Serialize)]
pub struct DependentRow {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub depth: u32,
    pub approx: bool,
    /// V15 Feature 3: weakest edge confidence along the discovery chain
    /// (`extracted`/`inferred`/`ambiguous`).
    pub confidence: String,
}

/// V12 Phase B (Analyses): the working-tree diff's blast radius — the
/// changed symbols, their transitive dependents, and any changed files the
/// graph doesn't index (docs/configs/etc.).
#[derive(serde::Serialize)]
pub struct ImpactResult {
    pub changed: Vec<ChangedSymbolRow>,
    pub dependents: Vec<DependentRow>,
    pub unindexed: Vec<String>,
}

/// V12 Phase B (Analyses): "what does my current working-tree change
/// affect?" — diff mode only (the `symbols`-scoped mode is MCP-tool only,
/// where an agent supplies explicit roots). `root` defaults to the launch
/// directory. Errors with a "requires git" message when `root` isn't a git
/// repository (see `AppError::NotAGitRepo`).
#[tauri::command]
pub async fn graph_impact(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<ImpactResult> {
    let root = resolve_graph_root(root)?;
    let report = service.impact(&root)?;
    Ok(ImpactResult {
        changed: report
            .changed
            .into_iter()
            .map(|s| ChangedSymbolRow {
                name: s.name,
                kind: s.kind,
                file: s.file,
                line: s.start_line,
            })
            .collect(),
        dependents: report
            .dependents
            .into_iter()
            .map(|d| DependentRow {
                name: d.symbol.name,
                kind: d.symbol.kind,
                file: d.symbol.file,
                line: d.symbol.start_line,
                depth: d.depth,
                approx: d.approx,
                confidence: d.confidence.tag().to_string(),
            })
            .collect(),
        unindexed: report.unindexed,
    })
}

// ── V15 Feature 1: path tracing ──────────────────────────────────────────

/// One node on a traced path, serialized for the Code Intelligence tab.
#[derive(serde::Serialize)]
pub struct PathNodeRow {
    pub id: String,
    pub label: String,
    pub file: String,
    pub line: u32,
    pub kind: String,
    pub edge_to_next: Option<String>,
    pub confidence: Option<String>,
}

/// The result of a `graph_path` trace. `found=false` means no path within the
/// hop bound (or an unresolvable endpoint).
#[derive(serde::Serialize)]
pub struct PathResult {
    pub found: bool,
    pub nodes: Vec<PathNodeRow>,
    pub hops: usize,
    pub equal_alternatives: u64,
}

fn parse_path_kinds(kinds: Option<Vec<String>>) -> Vec<crate::graph::EdgeKind> {
    use crate::graph::EdgeKind;
    let all = || vec![EdgeKind::Call, EdgeKind::Import, EdgeKind::Contains];
    let Some(ks) = kinds else { return all() };
    let mut out = Vec::new();
    for k in ks {
        match k.trim().to_ascii_lowercase().as_str() {
            "call" => out.push(EdgeKind::Call),
            "import" => out.push(EdgeKind::Import),
            "contains" => out.push(EdgeKind::Contains),
            _ => {}
        }
    }
    if out.is_empty() {
        all()
    } else {
        out
    }
}

/// V15 Feature 1 (Architecture): trace the shortest path between two entities
/// through the call/import/containment graph. `root` defaults to the launch
/// directory.
#[tauri::command]
pub async fn graph_path(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    from: String,
    to: String,
    kinds: Option<Vec<String>>,
    symmetric: Option<bool>,
) -> AppResult<PathResult> {
    let root = resolve_graph_root(root)?;
    let kinds = parse_path_kinds(kinds);
    let hit = service.shortest_path(
        &root,
        from.trim(),
        to.trim(),
        &kinds,
        symmetric.unwrap_or(false),
    )?;
    Ok(match hit {
        Some(h) => PathResult {
            found: true,
            nodes: h
                .nodes
                .into_iter()
                .map(|n| PathNodeRow {
                    id: n.id,
                    label: n.label,
                    file: n.file,
                    line: n.line,
                    kind: n.kind,
                    edge_to_next: n.edge_to_next,
                    confidence: n.confidence.map(|c| c.tag().to_string()),
                })
                .collect(),
            hops: h.hops,
            equal_alternatives: h.equal_alternatives,
        },
        None => PathResult {
            found: false,
            nodes: Vec::new(),
            hops: 0,
            equal_alternatives: 0,
        },
    })
}

// ── V15 Feature 2: architecture overview ─────────────────────────────────

#[derive(serde::Serialize)]
pub struct GodNodeRow {
    pub id: String,
    pub label: String,
    pub file: String,
    pub kind: String,
    pub degree: u64,
}

#[derive(serde::Serialize)]
pub struct SubsystemRow {
    pub name: String,
    pub size: usize,
    pub files: Vec<String>,
    pub hub: String,
}

#[derive(serde::Serialize)]
pub struct SurprisingRow {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub from_subsystem: String,
    pub to_subsystem: String,
}

#[derive(serde::Serialize)]
pub struct ArchResult {
    pub god_nodes: Vec<GodNodeRow>,
    pub subsystems: Vec<SubsystemRow>,
    pub surprising: Vec<SurprisingRow>,
}

/// V15 Feature 2 (Architecture): the system-shape overview — god nodes,
/// subsystems, and surprising cross-subsystem edges. `root` defaults to the
/// launch directory.
#[tauri::command]
pub async fn graph_architecture(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<ArchResult> {
    let root = resolve_graph_root(root)?;
    let r = service.architecture(&root)?;
    Ok(ArchResult {
        god_nodes: r
            .god_nodes
            .into_iter()
            .map(|g| GodNodeRow {
                id: g.id,
                label: g.label,
                file: g.file,
                kind: g.kind,
                degree: g.degree,
            })
            .collect(),
        subsystems: r
            .subsystems
            .into_iter()
            .map(|s| SubsystemRow {
                name: s.name,
                size: s.size,
                files: s.files,
                hub: s.hub,
            })
            .collect(),
        surprising: r
            .surprising
            .into_iter()
            .map(|e| SurprisingRow {
                from: e.from,
                to: e.to,
                kind: e.kind,
                from_subsystem: e.from_subsystem,
                to_subsystem: e.to_subsystem,
            })
            .collect(),
    })
}

// ── V15 Feature 4: Graph View snapshot ───────────────────────────────────

#[derive(serde::Serialize)]
pub struct VizNodeRow {
    pub id: String,
    pub label: String,
    pub file: String,
    pub kind: String,
    pub degree: u64,
    pub subsystem: String,
}

#[derive(serde::Serialize)]
pub struct VizEdgeRow {
    pub src: String,
    pub dst: String,
    pub kind: String,
    pub confidence: String,
    /// `false` = over the per-node drawn quota: listed/highlighted by the
    /// frontend but not rendered as an ambient line.
    pub drawn: bool,
}

#[derive(serde::Serialize)]
pub struct VizGraphResult {
    pub nodes: Vec<VizNodeRow>,
    pub edges: Vec<VizEdgeRow>,
}

/// V15 Feature 4 (Graph View): a bounded {nodes, edges} subgraph for the live
/// visualization (Tool Activity → Graph view). `root` defaults to the launch
/// directory.
#[tauri::command]
pub async fn graph_viz_snapshot(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<VizGraphResult> {
    let root = resolve_graph_root(root)?;
    let g = service.viz_snapshot(&root)?;
    Ok(VizGraphResult {
        nodes: g
            .nodes
            .into_iter()
            .map(|n| VizNodeRow {
                id: n.id,
                label: n.label,
                file: n.file,
                kind: n.kind,
                degree: n.degree,
                subsystem: n.subsystem,
            })
            .collect(),
        edges: g
            .edges
            .into_iter()
            .map(|e| VizEdgeRow {
                src: e.src,
                dst: e.dst,
                kind: e.kind,
                confidence: e.confidence,
                drawn: e.drawn,
            })
            .collect(),
    })
}

/// Per-file Graph View presence (Workbench ⌖ button state).
#[derive(serde::Serialize)]
pub struct VizFileStatusRow {
    pub path: String,
    /// The file exists in the graph index at all.
    pub indexed: bool,
    /// Rolled-up file-level call/import degree (0 = nothing to jump to).
    pub degree: u64,
}

/// Workbench ⌖ support: per-file Graph View presence for a batch of
/// repo-relative paths — the jump button disables for unindexed or
/// connection-less files. `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_viz_file_status(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    paths: Vec<String>,
) -> AppResult<Vec<VizFileStatusRow>> {
    let root = resolve_graph_root(root)?;
    Ok(service
        .viz_file_status(&root, &paths)?
        .into_iter()
        .map(|s| VizFileStatusRow {
            path: s.path,
            indexed: s.indexed,
            degree: s.degree,
        })
        .collect())
}

/// Workbench ⌖ support: the 1-hop FILE ego of `path` regardless of the
/// snapshot's top-N-by-degree cut — the Graph View injects it temporarily
/// when a jump targets a file the rendered snapshot dropped. `root` defaults
/// to the launch directory.
#[tauri::command]
pub async fn graph_viz_ego(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    path: String,
) -> AppResult<VizGraphResult> {
    let root = resolve_graph_root(root)?;
    let g = service.viz_ego(&root, &path)?;
    Ok(VizGraphResult {
        nodes: g
            .nodes
            .into_iter()
            .map(|n| VizNodeRow {
                id: n.id,
                label: n.label,
                file: n.file,
                kind: n.kind,
                degree: n.degree,
                subsystem: n.subsystem,
            })
            .collect(),
        edges: g
            .edges
            .into_iter()
            .map(|e| VizEdgeRow {
                src: e.src,
                dst: e.dst,
                kind: e.kind,
                confidence: e.confidence,
                drawn: e.drawn,
            })
            .collect(),
    })
}

/// V10 (Memory): the project's session/action memory — current session, its
/// working set, notes (pinned + current-session), and the recent-sessions list.
/// `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_memory(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<crate::graph::MemorySnapshot> {
    let root = resolve_graph_root(root)?;
    Ok(service.memory_snapshot(&root))
}

/// V10 (Memory): clear one session's memory (`session` = its id) or the whole
/// project's memory (`session` omitted). `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_memory_clear(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    session: Option<String>,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    let session = session.filter(|s| !s.trim().is_empty());
    service.mem_clear(&root, session.as_deref())
}

/// V10 (Memory): pin/unpin a note (pinned notes survive session eviction and
/// show project-wide). `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_note_set_pinned(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    note_id: String,
    pinned: bool,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    service.mem_set_note_pinned(&root, &note_id, pinned)
}

/// V32 Phase C2 (Memory): resolve one QUARANTINED note — `action` is
/// `"promote"` (clear the taint; the note becomes ordinary memory, pinned state
/// preserved) or `"discard"` (delete it). `root` defaults to the launch
/// directory.
///
/// Shaped like [`graph_fact_update`] rather than as two commands: the two
/// actions are the two halves of one review decision, always rendered side by
/// side, and an unknown `action` is rejected here rather than silently ignored —
/// a typo must not read as "reviewed, nothing happened" on a security control.
#[tauri::command]
pub async fn graph_note_review(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    note_id: String,
    action: String,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    match action.as_str() {
        "promote" => service.mem_promote_note(&root, &note_id),
        "discard" => service.mem_delete_note(&root, &note_id),
        other => Err(AppError::Graph(format!(
            "unknown note review action `{other}` (expected \"promote\" or \"discard\")"
        ))),
    }
}

/// V12 Phase E (Memory): the project's durable facts (pinned first, then
/// newest), excluding archived ones. `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_facts(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::graph::ProjectFact>> {
    let root = resolve_graph_root(root)?;
    Ok(service.list_project_facts(&root, false, 200))
}

/// V12 Phase E (Memory): pin / unpin / archive / delete one project fact.
/// `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_fact_update(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    id: String,
    action: String,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    match action.as_str() {
        "pin" => service.set_fact_pinned(&root, &id, true),
        "unpin" => service.set_fact_pinned(&root, &id, false),
        "archive" => service.set_fact_archived(&root, &id, true),
        "delete" => service.delete_fact(&root, &id),
        other => Err(crate::error::AppError::Graph(format!(
            "unknown fact action: {other} (expected pin|unpin|archive|delete)"
        ))),
    }
}

/// V12 Phase E (Memory): manually add a project fact from the Facts UI's "add
/// fact" input (recorded with `source_session = "manual"`). `root` defaults
/// to the launch directory.
#[tauri::command]
pub async fn graph_fact_add(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    text: String,
    pin: Option<bool>,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    service.add_project_fact_manual(&root, &text, pin.unwrap_or(false))
}

/// V10 (Context): preview what context injection WOULD prepend for `prompt`,
/// bypassing the `context_injection` toggle (so the user can tune before
/// enabling). Requires the graph to be enabled. `root` defaults to the launch
/// directory; no `session_id` (the preview isn't tied to a live session).
#[tauri::command]
pub async fn graph_context_preview(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    prompt: String,
    root: Option<String>,
) -> AppResult<crate::graph::RetrieveResult> {
    let root = resolve_graph_root(root)?;
    Ok(service.retrieve_context(&root, &prompt, None))
}

// ── V14 Phase D/D2: Usage section (token X-ray) + budget-tuning advisor ───

/// V14 Phase D: the Usage section's full payload for `root` — the current
/// session's per-turn series + top-tools ranking, every known session's
/// totals row, and the effectiveness counters. `root` defaults to the launch
/// directory.
#[tauri::command]
pub async fn graph_usage(
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    offload: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
    root: Option<String>,
) -> AppResult<crate::graph::UsageSnapshot> {
    let root = resolve_graph_root(root)?;
    // `usage_snapshot` is a multi-query Cozo pass measured in seconds against a
    // large store, and the Overview polls it on a timer — so it runs on the
    // blocking pool. Left on a runtime worker it parked one for the whole pass
    // and every other IPC queued behind it, which is what made switching tabs
    // feel sluggish while the dashboard was open.
    let graph = graph.inner().clone();
    let offload = offload.inner().clone();
    run_on_blocking_pool(move || {
        let mut snap = graph.usage_snapshot(&root);
        // Offload local-task count: completed runs (not still `"running"`) on
        // `local` backends only — "N tasks served locally" per the milestone's
        // Effectiveness panel, distinct from a run still in flight. GraphService
        // has no dependency on OffloadService, so this is filled in here rather
        // than inside `usage_snapshot`.
        snap.offload_local_tasks = offload
            .server_metrics()
            .into_iter()
            .filter(|b| b.kind == "local")
            .flat_map(|b| b.metrics.runs)
            .filter(|r| r.outcome != "running")
            .count() as u64;
        // V17 Phase E: the advertised tool-surface size (both consumers), measured
        // post-`lean_tools`-filter from live settings — another cross-cutting field
        // GraphService can't fill (it depends on settings, not the index).
        snap.surface = crate::graph::surface_stats();
        snap
    })
    .await
}

/// V24 Phase B: full drill-in detail for ONE session under `root` — its totals
/// row, per-turn series, top-tools ranking, and per-model token totals with the
/// session/agent origin split. Unlike `graph_usage` (which only surfaces the
/// current session at full detail), this works for any session id, so the Usage
/// card can render a clicked historical session. An unknown session id returns
/// an empty detail (no error, no panic). `root` defaults to the launch
/// directory.
#[tauri::command]
pub async fn graph_session_usage(
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    session_id: String,
) -> AppResult<crate::graph::SessionUsageDetail> {
    let root = resolve_graph_root(root)?;
    // Same store-pass cost profile as `graph_usage` — off the runtime workers.
    let graph = graph.inner().clone();
    run_on_blocking_pool(move || graph.session_usage_detail(&root, &session_id)).await
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
#[tauri::command]
pub async fn graph_tab_session(
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    tab: String,
) -> AppResult<Option<String>> {
    Ok(graph.live_session_for_any_agent(&tab))
}

/// V14 Phase D2: the `graph_usage_advice` response. Wraps `advisor::evaluate`'s
/// `Vec<Proposal>` with a `collecting` flag — NOT part of the milestone's
/// literal `Vec<Proposal>` pseudocode, added because the Advisor card (D2.4)
/// needs to distinguish "no data yet" from "checked, all healthy", and a
/// bare `Vec<Proposal>` can't carry that distinction on its own.
#[derive(serde::Serialize)]
pub struct AdvisorSnapshot {
    pub proposals: Vec<crate::advisor::Proposal>,
    pub collecting: bool,
}

/// Count calls to any `graph::LEAN_HIDDEN` tool in `activity` within the
/// trailing `window_ms` ending at `now_ms` — the `hideable_tool_calls` signal
/// feeding `surface.lean.v1`. Zero ⇒ the lean-surface rule may fire.
/// `now_ms.saturating_sub(window_ms)` is the inclusive cutoff, so entries older
/// than the window (including ancient residue in the count-capped ring) don't
/// count. Free function so the window semantics stay unit-testable apart from
/// the IPC command.
fn count_hideable_tool_calls(
    activity: &[crate::activity::ActivityEntry],
    now_ms: u64,
    window_ms: u64,
) -> u64 {
    let cutoff = now_ms.saturating_sub(window_ms);
    activity
        .iter()
        .filter(|e| e.ts_ms >= cutoff && crate::graph::LEAN_HIDDEN.contains(&e.tool.as_str()))
        .count() as u64
}

/// Every registered harness's [`crate::advisor::DriftSignals`], for one advisor
/// poll (V40 Phase C, locked decision 23).
///
/// The version half — `last_seen`, `last_verified`, `auto_verify` — is genuinely
/// per harness: it comes out of `Settings::harness[<id>]`, which Phase B made a
/// map, so a second harness gets a real `drift.version.v1` path for the first
/// time.
///
/// **The SESSION half is per harness too since V40 Phase D** (locked decision
/// 20). It used to be filled for the default harness only, with zeros for every
/// other, because the queries behind it had one agent literal inside them —
/// `drift.usage_fields_gone.v1` therefore never tripped its sample floor for a
/// second harness, and a rule that cannot fire looks exactly like a rule that
/// found nothing. `sessions` / `tokenless_sessions` now come from
/// `GraphIndex::tokenless_sessions(agent)` run once per registered harness, and
/// `subagent_drift` from the Activity rows each plugin declares it files
/// (`drift_report_tools`). A harness whose reader files no drift reports gets an
/// empty list — the truth, rather than a zero-fill that looks like one.
fn harness_drift_signals(
    sessions: &std::collections::BTreeMap<crate::harness::HarnessId, (u64, u64)>,
    subagent_drift: &std::collections::BTreeMap<crate::harness::HarnessId, Vec<String>>,
) -> crate::advisor::HarnessDriftSignals {
    let map = crate::settings::read_global_harness_map();
    crate::harness::registry::all()
        .map(|id| {
            let row = map
                .get(id.token())
                .cloned()
                .unwrap_or_else(|| crate::settings::read_global_harness_settings(id));
            let (sessions, tokenless_sessions) = sessions.get(&id).copied().unwrap_or((0, 0));
            (
                id,
                crate::advisor::DriftSignals {
                    last_seen: row.last_seen,
                    last_verified: row.last_verified,
                    auto_verify: row.auto_verify,
                    sessions,
                    tokenless_sessions,
                    subagent_drift: subagent_drift.get(&id).cloned().unwrap_or_default(),
                },
            )
        })
        .collect()
}

/// V14 Phase D2: the budget-tuning advisor's current proposals for `root`.
/// Assembled fresh on every call from `GraphService`'s D2.1 signal getters —
/// cheap (bounded Datalog queries + a small in-memory scan), no caching
/// needed. `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_usage_advice(
    state: State<'_, AppState>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<AdvisorSnapshot> {
    let root = resolve_graph_root(root)?;
    let settings = state.settings.current();
    // A dozen bounded Datalog queries against the same single-connection store,
    // on the Overview's poll cadence — off the runtime workers, same reasoning
    // as `graph_usage`. The body is a plain sync fn (rather than an inline
    // closure) so it stays readable and its `root`/`settings` stay owned.
    let graph = graph.inner().clone();
    run_on_blocking_pool(move || advisor_snapshot_blocking(&graph, root, settings)).await
}

/// The blocking body of [`graph_usage_advice`] — every signal read plus the
/// `advisor::evaluate` call. Split out only so the command can hand it to
/// [`run_on_blocking_pool`]; `root` and `settings` are owned because the
/// closure that carries them must be `'static`.
fn advisor_snapshot_blocking(
    graph: &crate::graph::GraphService,
    root: std::path::PathBuf,
    settings: crate::settings::Settings,
) -> AdvisorSnapshot {
    let (injection_follow_rate, injection_follow_samples) = match graph.injection_follow_rate(&root)
    {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let (budget_maxed_rate, budget_maxed_samples) = match graph.budget_maxed_rate(&root) {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let (advisor_reread_rate, advisor_reread_samples) = match graph.advisor_reread_rate(&root) {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let session_count = graph.advisor_session_count(&root);

    // V16 drift signals. `harness_versions` is read from the physical global
    // file (not the live merged settings) so background writes — the tap
    // noting a version mid-run — are visible without a restart (mtime-cached,
    // so the 2s poll doesn't re-parse the file every tick).
    let hv = crate::settings::read_global_harness_versions();
    // `remind_count` (drift.read_hook_silent.v1) is the same total-remind-rows
    // count `advisor_reread_rate` just scanned for — reuse its sample count
    // instead of a second identical Datalog scan.
    let remind_count = advisor_reread_samples;
    let (large_reread_pairs, sessions_by_harness) = graph.drift_db_signals(&root);
    // One clone of the activity ring serves both the bypass-rate signal and
    // the contract-drift filter.
    let activity = crate::activity::snapshot();
    let (bypass_rate, bypass_samples) = match graph.bypass_rate(&activity) {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let since = crate::activity::process_start_ms();
    let contract_drift: Vec<String> = activity
        .iter()
        .filter(|e| e.source == "harness" && e.tool == "contract_drift" && e.ts_ms >= since)
        .map(|e| e.target.clone())
        .collect();
    // V17.1: sub-agent transcript-contract drift reports filed by a harness's
    // own reader — same channel discipline as the `contract_drift` events
    // above. V40 Phase D: attributed to the harness that files them, which each
    // plugin declares (`drift_report_tools`), because the rule that reads them
    // runs per harness. A harness whose reader files none has an empty list —
    // which is the truth, not a zero-fill.
    let subagent_drift_by_harness: std::collections::BTreeMap<crate::harness::HarnessId, Vec<String>> =
        crate::harness::registry::all()
            .map(|h| {
                let tools = h.plugin().map(|p| p.drift_report_tools()).unwrap_or(&[]);
                let rows = activity
                    .iter()
                    .filter(|e| {
                        e.source == "harness"
                            && e.ts_ms >= since
                            && tools.contains(&e.tool.as_str())
                    })
                    .map(|e| e.target.clone())
                    .collect();
                (h, rows)
            })
            .collect();

    // V17 Phase E signals: RECENT calls to any lean-hidden tool in the Activity
    // ring (zero ⇒ the lean-surface rule may fire) and the measured advertised
    // surface size for its rationale. Unlike the drift filters above, this uses
    // a trailing recency window rather than process-start `since`: the ring is
    // count-capped (GRAPH_CAP/OFFLOAD_CAP), so an all-time scan would let one
    // cold-tail call weeks ago suppress the suggestion forever, while
    // process-start would flip a tool to "unused" minutes after every restart.
    let hideable_tool_calls = count_hideable_tool_calls(
        &activity,
        crate::activity::now_ms(),
        crate::advisor::HIDEABLE_RECENCY_WINDOW_MS,
    );
    let surface_chars = crate::graph::surface_stats().mcp_chars as u64;

    // V17 Phase F1/F2 signals. Redundant re-read pairs per session over the
    // last 10 sessions, sized by the current advisor line floor. `e1_pass` is
    // STRICTLY the "pass" status (trimmed/lowercased) — NOT "the
    // `claude.hook.pretooluse_deny` gate is not blocking"
    // (`harness::contract::gate`, which passes `"unverified"` as well), so an
    // "unverified" E1 (the default) never auto-graduates a hook we've never
    // proven works. V35 Phase E retired the gate helper this used to be
    // contrasted with and left this check untouched.
    let (redundant_reads_per_session, redundant_read_sessions) = match graph
        .redundant_read_candidates(&root, settings.graph.read_advisor_min_lines, 10)
    {
        Some((pairs, sessions)) if sessions > 0 => (Some(pairs as f64 / sessions as f64), sessions),
        _ => (None, 0),
    };
    let e1_pass = hv.e1_status.trim().eq_ignore_ascii_case("pass");

    // V32 Phase C3: the detection updater's three canaries (a newer bundle
    // offered, a bundle refused, a channel that has been unreachable for a week
    // — #46 split the last one out of the second). Read from its in-memory
    // state cache — no disk and no clock, so this is safe on the advice poll's
    // cadence — and unlike every other signal here they are not per-root: the
    // detection data is process-wide, so the same card shows in whichever
    // project the user happens to have open.
    let (detection_updates, detection_update_failures, detection_update_stalled) =
        crate::offload::detection::updater::advisor_signals();
    // #48/D-2: the fourth detection canary, and the only one about the data on
    // disk rather than the channel — the signature layer switched on with
    // nothing to match against. Reads the cached compile report (no disk, no
    // clock) and resolves the layer's own switch through the injection
    // hierarchy, so a layer the user turned off says nothing.
    let detection_signature_down = crate::offload::detection::signature::advisor_signal(&settings);
    // #48/U-4: the fifth — a rule file the USER wrote that does not compile.
    // Its own signal rather than a widening of the one above, because the two
    // are different states (skipped file vs. disarmed layer) with different
    // fixes; the updater suppresses this one while that one is up.
    let detection_local_rules_broken =
        crate::offload::detection::updater::broken_local_rules(&settings);
    // #48/M-11: the sixth — the live rule directory is SHORT of files a
    // rollback could not put back. Deliberately not gated on the detection
    // switch: the files are missing from disk whether or not the layer is
    // currently screening with them, and a user who switches detection back on
    // must not silently get a short set.
    let detection_rules_incomplete = crate::offload::detection::updater::rules_incomplete();

    // Apply-cooldown records are stored per (rule, root) — hand `evaluate`
    // only THIS root's, so an Apply in one project never mutes another
    // (whose own session count may be far lower). Both this filter and the
    // writer (`advisor_mark_applied`) derive the string from
    // `resolve_graph_root`, so the forms compare equal.
    let root_str = root.to_string_lossy().to_string();
    let applied: Vec<crate::settings::AppliedRule> = settings
        .advisor_applied
        .iter()
        .filter(|a| crate::activity::root_key_eq(&a.root, &root_str))
        .cloned()
        .collect();

    let sig = crate::advisor::Signals {
        injection_follow_rate,
        injection_follow_samples,
        advisor_reread_rate,
        advisor_reread_samples,
        budget_maxed_rate,
        budget_maxed_samples,
        session_count,
        graph: settings.graph.clone(),
        dismissed: settings.advisor_dismissed.clone(),
        applied,
        // V40 Phase C, locked decision 23: ONE ROW PER REGISTERED HARNESS,
        // read from the same fresh physical-global snapshot (the auto-verify
        // worker writes the version half out of band, so a record a second old
        // must be visible to the very next 2 s advisor poll without a restart).
        //
        // Phase B moved the storage into `harness[<id>]` and left a note here
        // saying the reader still took the DEFAULT harness's row because every
        // V16 rule was written around Claude's payload shapes. The rules are
        // per-harness now, so this is the whole map.
        harness: harness_drift_signals(&sessions_by_harness, &subagent_drift_by_harness),
        remind_count,
        large_reread_pairs,
        contract_drift,
        bypass_rate,
        bypass_samples,
        hideable_tool_calls,
        surface_chars,
        redundant_reads_per_session,
        redundant_read_sessions,
        e1_pass,
        detection_updates,
        detection_update_failures,
        detection_update_stalled,
        detection_signature_down,
        detection_local_rules_broken,
        detection_rules_incomplete,
    };
    let proposals = crate::advisor::evaluate(&sig);
    // "Collecting" = nothing has cleared the cold-start floor yet: not
    // enough sessions, OR neither of the two independent sample counts
    // (injections / reminders) has cleared its own rule's floor. Distinct
    // from "cleared the floor, rates are just healthy" (empty proposals,
    // `collecting = false`). V16: drift canaries carry their OWN floors and
    // can fire below the tuning floor (a version bump is a fact, not a
    // statistic) — a non-empty proposal list must therefore always render,
    // so `collecting` yields to it.
    let collecting = proposals.is_empty()
        && (session_count < crate::advisor::MIN_SESSIONS
            || (injection_follow_samples < crate::advisor::MIN_INJECTIONS
                && advisor_reread_samples < crate::advisor::MIN_REMINDS));
    AdvisorSnapshot {
        proposals,
        collecting,
    }
}

/// The Settings window's harness payload: the raw spike/version record plus
/// the **computed** gate verdicts (V35 Phase E).
///
/// `capability_gates` is a list of self-describing records rather than one
/// bespoke boolean per feature, and that is the whole point: before Phase E the
/// window received `e1_status` and re-implemented the fail-closed reading of it
/// in TypeScript (`harnessStatusBlocks`), so a change to the rule had to be
/// made twice or the toggle and the installed hook would disagree. Adding a
/// second bespoke flag here would have recreated exactly that. Phase G's
/// *Harness health* panel renders this same list.
#[derive(serde::Serialize)]
pub struct HarnessStatus {
    /// Fresh read of the physical global `harness_versions`.
    pub versions: crate::settings::HarnessVersions,
    /// Every gated capability's verdict, keyed by capability id — the same
    /// query `tabs/config.rs` asks before installing the hook.
    pub capability_gates: Vec<crate::harness::contract::Gate>,
    /// V35 Phase G: the whole *Harness health* read-model — every registry row
    /// with its tier, contract sentence, degradation, coverage marks, TCB
    /// controls, gate verdict and last check result, grouped by harness and
    /// ordered riskiest-tier-first.
    ///
    /// Served from THIS command rather than a sibling: it is the same fresh
    /// `harness_versions` read and the same `contract::gates` call the payload
    /// already makes, the command is called on Settings open (and while a run
    /// is in flight) rather than on any hot path, and a second command would
    /// mean two round trips that could disagree about the versions they were
    /// computed against.
    pub harness_health: Vec<crate::harness::health::HarnessHealth>,
    /// A verify run is happening right now, so *Run checks now* is a no-op and
    /// the panel should keep polling.
    pub verify_in_flight: bool,
    /// V40 Phase F (locked decision 27): the gated capability ids, keyed by the
    /// neutral CONTROL each one gates (`harness::contract::GATED_CONTROLS`).
    ///
    /// The window used to hold one of these ids — a harness-namespaced hook
    /// name — as a TypeScript constant so it could join on it. It looks the id
    /// up here now, so a gate whose capability belongs to a harness reaches the
    /// frontend as data rather than as a second spelling.
    pub gated_controls: std::collections::BTreeMap<&'static str, &'static str>,
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
#[tauri::command]
pub async fn harness_list() -> AppResult<Vec<crate::harness::info::HarnessInfo>> {
    Ok(crate::harness::info::harness_list())
}

/// V16 Feature 1: the harness version + contract-verification state, read
/// from the physical global `settings.json` (fresh — background writers
/// bypass the live settings snapshot).
///
/// V35 Phase E: the gates are computed against the live settings with the FRESH
/// `harness_versions` layered in, so a hand-recorded spike outcome disables the
/// toggle without an app restart — the reason this command exists at all.
#[tauri::command]
pub async fn harness_versions_get(state: State<'_, AppState>) -> AppResult<HarnessStatus> {
    let versions = crate::settings::read_global_harness_versions();
    let mut settings = state.settings.current();
    settings.harness_versions = versions.clone();
    // V40 Phase B: the versions, the auto-verify records and the recorded spike
    // outcomes all live in `harness` now, and all three are written out of band
    // — so the panel has to be computed against a FRESH read of that map for
    // exactly the reason it already was for `harness_versions`.
    settings.harness = crate::settings::read_global_harness_map();
    Ok(HarnessStatus {
        capability_gates: crate::harness::contract::gates(&settings),
        // V35 Phase G: computed against the SAME fresh-versions settings as the
        // gates, so the panel's headers, its gate badges and its last-verified
        // dates are one consistent reading rather than three.
        harness_health: crate::harness::health::health(&settings),
        verify_in_flight: crate::harness::verify::in_flight(),
        versions,
        gated_controls: crate::harness::contract::GATED_CONTROLS
            .iter()
            .copied()
            .collect(),
    })
}

/// V35 Phase G: the *Harness health* panel's one action — run this harness's
/// L1 canaries and L2 probes now.
///
/// Returns whether a run STARTED. `false` means one was already in flight (a
/// second click, or an automatic run triggered by a version change) and this
/// request was dropped rather than queued; the panel shows the in-flight state
/// either way and re-reads `harness_versions_get` when it clears.
///
/// Fire-and-forget by construction: the work spawns a blocking OS thread that
/// drives child processes for up to 90s, so the command returns as soon as the
/// thread is up. The result arrives through the payload above — for Claude via
/// the Phase F record (the same write path the automatic run uses), for every
/// harness via the in-memory run summary.
#[tauri::command]
pub async fn harness_run_checks(harness: String) -> AppResult<bool> {
    // Resolved through the probe's own token table rather than a `match` here:
    // the panel renders `HarnessHealth::harness`, which IS that table's output,
    // so round-tripping through it is what keeps the button pointed at the
    // harness whose header it sits under.
    let h = crate::harness::probe::harness_from_name(harness.trim()).ok_or_else(|| {
        AppError::Ipc(format!(
            "harness_run_checks: {harness:?} is not a harness that can be run"
        ))
    })?;
    Ok(crate::harness::verify::run_now(h))
}

/// V16 Feature 1: the Advisor card's "Mark verified" action — stamp the
/// currently-seen version of `harness` as the last-verified one (the user just
/// re-ran the MAINTENANCE.md contract checks). Also mirrors the change into the
/// live settings so the open Settings window sees it without a restart.
///
/// **V40 Phase B: it takes a harness.** It used to write `claude_last_verified`
/// with no argument at all, so the OpenCode row of the health panel had no
/// action that could ever clear it. `None` is the DEFAULT harness — the
/// documented wire-compatibility default (locked decision 22), which keeps the
/// existing frontend call site working unchanged until Phase F passes the id
/// the button sits under.
#[tauri::command]
pub async fn harness_mark_verified(
    state: State<'_, AppState>,
    harness: Option<String>,
) -> AppResult<()> {
    let id = match harness.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => crate::harness::HarnessId::from_id(name).ok_or_else(|| {
            AppError::Ipc(format!("harness_mark_verified: {name:?} is not a harness"))
        })?,
        None => crate::harness::DEFAULT_HARNESS,
    };
    let after = crate::settings::mutate_global_harness(id, |row| {
        row.last_verified = row.last_seen.clone();
    })?;
    let key = id.token().to_string();
    state.settings.mutate(move |cur| {
        cur.harness.insert(key.clone(), after.clone());
    });
    Ok(())
}

/// **The model-visible text one tab's harness receives**, keyed by slot (V40
/// Phase E, locked decision 24).
///
/// The compose overlay is the first consumer: it appends one instruction line
/// after the `[image] <path>` lines it types into the tab, and that line used to
/// be a literal in `compose/attachments.ts` — a string the model reads that
/// nothing in the backend inventory could see, and that no harness could
/// influence. It comes over this command now.
///
/// `tab` is a tab id; a tab that runs no registered harness (or an unknown id)
/// gets the NEUTRAL rendering, which is a real answer rather than a failure —
/// the same posture `instructions::all_for` takes.
#[tauri::command]
pub async fn harness_instructions(
    state: State<'_, AppState>,
    tab: Option<String>,
) -> AppResult<std::collections::BTreeMap<String, String>> {
    let settings = state.settings.current();
    let harness = tab
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .and_then(|t| crate::tabs::tab_harness_by_id(&settings, t));
    Ok(crate::harness::instructions::all_for(harness)
        .iter()
        .map(|i| (i.slot.id().to_string(), i.text.to_string()))
        .collect())
}

/// **The advisor's rule reference** (V40 Phase F, locked decision 23).
///
/// The Code Intelligence panel used to hold this table as a hard-coded tooltip
/// — a restatement of thresholds `advisor.rs` owns, with one harness's
/// mechanisms named in it for rules that fire per registered harness. It
/// renders this instead.
///
/// `'static` data; the window fetches it once when the panel first opens.
#[tauri::command]
pub async fn advisor_rules() -> AppResult<AdvisorRules> {
    Ok(AdvisorRules {
        rules: crate::advisor::RULE_REFERENCE.to_vec(),
        footer: crate::advisor::RULE_REFERENCE_FOOTER,
    })
}

/// The answer [`advisor_rules`] gives.
#[derive(serde::Serialize)]
pub struct AdvisorRules {
    /// One row per rule, in the order the reference lists them.
    pub rules: Vec<crate::advisor::RuleReference>,
    /// The one sentence that is about the panel rather than about a rule.
    pub footer: &'static str,
}

/// V14 Phase D2: dismiss one advisor proposal (`rule_id` + its coarse rate
/// `signature`, both echoed from the `Proposal` the user clicked Dismiss
/// on). Persisted in `Settings.advisor_dismissed`; a materially changed rate
/// (a different signature bucket) re-fires the proposal even for the same
/// `rule_id`. Idempotent — dismissing the same pair twice is a no-op.
#[tauri::command]
pub async fn advisor_dismiss(
    state: State<'_, AppState>,
    rule_id: String,
    signature: String,
) -> AppResult<()> {
    state.settings.mutate(move |cur| {
        let already = cur
            .advisor_dismissed
            .iter()
            .any(|d| d.rule_id == rule_id && d.signature == signature);
        if !already {
            cur.advisor_dismissed
                .push(crate::settings::DismissedRule { rule_id, signature });
        }
    });
    Ok(())
}

/// Record that the user APPLIED an advisor proposal, starting the rule's
/// Apply cooldown (`advisor::APPLY_COOLDOWN_SESSIONS` sessions of quiet so
/// fresh post-change data can accumulate before the rule re-evaluates — the
/// rates are cumulative, and an immediate re-proposal would be judging the
/// OLD value's data). Captures the root's session count server-side at call
/// time; one record per (rule, root), re-applying replaces it. Called by
/// the Advisor card's Apply right after the `settings_update` that writes
/// the proposed value — the settings write itself stays the ordinary path
/// (never silent self-modification).
#[tauri::command]
pub async fn advisor_mark_applied(
    state: State<'_, AppState>,
    graph: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
    rule_id: String,
) -> AppResult<()> {
    let root = resolve_graph_root(root)?;
    let session_count = graph.advisor_session_count(&root);
    let root_str = root.to_string_lossy().to_string();
    state.settings.mutate(move |cur| {
        cur.advisor_applied
            .retain(|a| {
                !(a.rule_id == rule_id && crate::activity::root_key_eq(&a.root, &root_str))
            });
        cur.advisor_applied.push(crate::settings::AppliedRule {
            rule_id,
            root: root_str,
            session_count,
        });
    });
    Ok(())
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
/// the frontend calls it on tab open and after a rebuild, not on a poll.
#[tauri::command]
pub async fn graph_language_census(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<Vec<crate::graph::LangCensus>> {
    let root = resolve_graph_root(root)?;
    Ok(service.language_census(&root))
}

/// V9-02: add or remove a language from the code graph's index set. Adds/removes
/// the tag in `GraphSettings.languages` (persisted), then kicks a full rebuild
/// so the change takes effect — indexing new files (and embedding them when
/// semantic search is on) or dropping the removed language's rows. Rejects
/// unsupported tags. `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_set_language_enabled(
    state: State<'_, AppState>,
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    lang: String,
    enabled: bool,
    root: Option<String>,
) -> AppResult<()> {
    let tag = lang.trim().to_ascii_lowercase();
    if crate::graph::Lang::from_tag(&tag) == crate::graph::Lang::Other {
        return Err(AppError::Settings(format!(
            "unsupported graph language: {lang}"
        )));
    }
    // Skip the mutate + full rebuild when the desired state already holds
    // (re-enabling an already-present language, or disabling an absent one).
    // A redundant rebuild re-indexes/re-embeds the whole project for nothing.
    let already = state
        .settings
        .current()
        .graph
        .languages
        .iter()
        .any(|l| l == &tag);
    if enabled == already {
        return Ok(());
    }
    state.settings.mutate(move |cur| {
        let langs = &mut cur.graph.languages;
        if enabled {
            langs.push(tag);
        } else {
            langs.retain(|l| l != &tag);
        }
    });
    let root = resolve_graph_root(root)?;
    // A Settings language toggle is a user action, like Rebuild.
    service.spawn_rebuild(root, crate::graph::RebuildOrigin::User);
    Ok(())
}

/// Open `<portable-root>/logs/content/` in the host file manager. Creates the
/// folder first if it doesn't exist so the call doesn't 404 on a clean
/// install. Windows uses `explorer.exe`; macOS `open`; Linux
/// `xdg-open`. Errors are wrapped in `AppError::Settings` for a single
/// IPC error type.
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
#[tauri::command]
pub async fn content_clear() -> AppResult<u32> {
    Ok(crate::content::delete_all())
}

#[tauri::command]
pub async fn list_voices() -> AppResult<Vec<String>> {
    use std::collections::BTreeSet;
    let mut out = BTreeSet::<String>::new();
    let dir = crate::tts::model_dir()?.join("voices");
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("bin") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    Ok(out.into_iter().collect())
}

#[tauri::command]
pub async fn open_settings_window(app: AppHandle) -> AppResult<()> {
    open_or_focus_settings(&app)
}

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
    if let Ok(mut slot) = state.pending_settings_deep_link.lock() {
        *slot = Some(tab.clone());
    }
    open_or_focus_settings(&app)?;
    let _ = app.emit_to(
        EventTarget::webview_window(SETTINGS_LABEL),
        "settings-deep-link",
        serde_json::json!({ "kind": "tab", "tab_id": tab }),
    );
    Ok(())
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
    if let Ok(mut slot) = state.pending_settings_deep_link.lock() {
        *slot = Some(format!("section:{section}"));
    }
    open_or_focus_settings(&app)?;
    let _ = app.emit_to(
        EventTarget::webview_window(SETTINGS_LABEL),
        "settings-deep-link",
        serde_json::json!({ "kind": "section", "section": section }),
    );
    Ok(())
}

/// V1.4-07 A: pulled by `SettingsApp.svelte` on mount to read+clear any
/// pending deep-link target stored by `open_settings_window_to_tab`.
/// Returns `None` when no target is pending.
#[tauri::command]
pub async fn consume_settings_deep_link(state: State<'_, AppState>) -> AppResult<Option<String>> {
    Ok(state
        .pending_settings_deep_link
        .lock()
        .ok()
        .and_then(|mut g| g.take()))
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

#[cfg(test)]
mod tests {
    use super::is_automatic_terminal_response as auto_reply;
    use super::read_only_refusal;
    use crate::settings::Settings;
    use crate::state::{ReadOnlySource, TabId};

    /// **"No quota source" and "records no turns" are two answers, and this
    /// command gives both** (V40 Phase G, locked decision 19).
    ///
    /// The regression this pins is the one the phase exists to remove: the
    /// declared token categories and turn lanes used to hang off `source`, so a
    /// harness that reports no quota was also declared to record no turns — and
    /// the Usage donut had no labels for its sessions' lanes. Live-verify 14
    /// reads the FIRST half of this (a harness answering *no usage source*, not
    /// a widget at 0%), so both halves are asserted together.
    ///
    /// Names no product: the two harnesses are picked out of the registry by
    /// what they DECLARE, which is what locked decision 10(a) asks of core.
    #[tokio::test]
    async fn harness_usage_reports_a_turn_shape_independently_of_a_quota_source() {
        let mut quota_only = 0usize;
        let mut turns_without_quota = 0usize;
        for id in crate::harness::registry::all() {
            let answer = super::harness_usage(id.token().to_string())
                .await
                .expect("a registered harness answers");
            let plugin = id.plugin();
            let has_source = plugin.and_then(|p| p.usage_source()).is_some();
            let has_shape = plugin.and_then(|p| p.turn_usage_shape()).is_some();
            assert_eq!(
                answer.source.is_some(),
                has_source,
                "{id}: the `source` half must mirror the declaration exactly"
            );
            assert_eq!(
                !answer.origins.is_empty(),
                has_shape,
                "{id}: the lanes must arrive whenever a turn shape is declared"
            );
            assert_eq!(
                !answer.token_kinds.is_empty(),
                has_shape,
                "{id}: the categories must arrive whenever a turn shape is declared"
            );
            if has_source {
                quota_only += 1;
                // The quota half carries WINDOWS and nothing else now.
                assert!(!answer.source.as_ref().unwrap().windows.is_empty());
            }
            if has_shape && !has_source {
                turns_without_quota += 1;
                // Exactly the case that was unrepresentable: no quota widget at
                // all, and still a labelled lane split for its stored rows.
                assert!(answer.reading.is_none(), "{id}: no source can produce no reading");
                assert!(
                    answer.origins.iter().any(|o| o.subagent),
                    "{id}: it rolls a child session's spend up, so it declares the lane"
                );
            }
        }
        assert!(quota_only > 0, "no harness declares a quota source at all");
        assert!(
            turns_without_quota > 0,
            "no harness records turns without reporting quota — if that becomes true, this \
             command's independence has no live example and the two fields can silently \
             re-couple"
        );
    }

    /// An unregistered harness id REJECTS rather than answering an empty shape.
    #[tokio::test]
    async fn harness_usage_rejects_an_unregistered_harness() {
        assert!(super::harness_usage("not-a-harness".to_string()).await.is_err());
    }

    /// **The facade knobs are a NARROW write** (V39 review M-10).
    ///
    /// The popover's old path sent the whole `Settings` document, which can
    /// revert a role change that landed after its snapshot was taken (the
    /// `40d2b32` class). What replaces it must touch the three knobs and
    /// nothing else — least of all `delegation_role`, whose cross-tab rule
    /// only `tab_set_delegation_role` enforces.
    #[test]
    fn the_backend_patch_touches_the_knobs_and_nothing_else() {
        use crate::settings::{BackendTier, DelegationBackend, DelegationRole, TabConfig};
        let mut tab = crate::settings::default_claude_tab();
        let TabConfig::AiTool(cfg) = &mut tab else {
            panic!("an AI tab");
        };
        cfg.delegation_role = DelegationRole::Manual;
        cfg.read_only = true;
        cfg.name = "api-work".to_string();
        let before = cfg.clone();

        super::apply_backend_patch(
            cfg,
            DelegationBackend {
                name: Some("lan-worker-2".to_string()),
                tier: BackendTier::Fast,
                declared_context: Some(128_000),
            },
        );

        assert_eq!(cfg.delegation_backend.name.as_deref(), Some("lan-worker-2"));
        assert_eq!(cfg.delegation_backend.tier, BackendTier::Fast);
        assert_eq!(cfg.delegation_backend.declared_context, Some(128_000));
        assert_eq!(
            cfg.delegation_role, before.delegation_role,
            "the role is the one field a knob write must never move"
        );
        assert_eq!(cfg.read_only, before.read_only);
        assert_eq!(cfg.name, before.name);
        assert_eq!(cfg.command, before.command);
    }

    /// Blank is unset, at the boundary: a cleared text field arrives as `""`
    /// and a cleared number field as `0`.
    #[test]
    fn a_cleared_knob_is_stored_as_absent_not_as_blank() {
        use crate::settings::DelegationBackend;
        let out = super::normalise_backend(DelegationBackend {
            name: Some("   ".to_string()),
            declared_context: Some(0),
            ..Default::default()
        });
        assert_eq!(out.name, None);
        assert_eq!(out.declared_context, None);
        let kept = super::normalise_backend(DelegationBackend {
            name: Some("  lan-worker-2 ".to_string()),
            declared_context: Some(64_000),
            ..Default::default()
        });
        assert_eq!(kept.name.as_deref(), Some("lan-worker-2"));
        assert_eq!(kept.declared_context, Some(64_000));
    }

    /// **The keyboard's submit rule is unchanged, and it is the wrong rule for
    /// a paste** (V39 review L-1).
    ///
    /// A person's Enter IS the submit, so `pty_write` still reads it off the
    /// bytes. A delegation's paste carries the request's own newlines, and
    /// under the same rule it read as a submit one write early — which is why
    /// the engine STATES it instead of letting it be inferred.
    #[test]
    fn a_submit_is_inferred_for_the_keyboard_and_stated_by_the_engine() {
        use super::Submit;
        assert_eq!(Submit::from_input("hello"), Submit::No);
        assert_eq!(Submit::from_input(""), Submit::No);
        assert_eq!(Submit::from_input("\r"), Submit::Yes);
        assert_eq!(Submit::from_input("hello\n"), Submit::Yes);
        // The trap, spelled out: a bracketed paste of a two-line request looks
        // exactly like a submit to the byte rule.
        let paste = "\u{1b}[200~line one\nline two\u{1b}[201~";
        assert_eq!(
            Submit::from_input(paste),
            Submit::Yes,
            "the byte rule cannot tell a pasted newline from a pressed Enter"
        );
    }

    /// **V39 Phase A: a refusal always names why.** The frontend shows this
    /// string verbatim in a toast, so an empty or generic one leaves the user
    /// staring at a tab that has silently stopped accepting keys.
    #[test]
    fn a_read_only_write_is_refused_with_the_reason_named() {
        assert_eq!(
            read_only_refusal(&ReadOnlySource::User, "hello", None).as_deref(),
            Some("read-only (user)")
        );
        assert_eq!(
            read_only_refusal(
                &ReadOnlySource::Driven {
                    by: TabId::from_str("opencode")
                },
                "hello",
                Some("api-work"),
            )
            .as_deref(),
            Some("driven by api-work")
        );
    }

    /// **The lock is on the user's keyboard, not on the terminal protocol.**
    /// A TUI that queried the cursor position must still get its answer while
    /// the tab is locked — otherwise the very tab a delegation is driving is
    /// the one that wedges.
    #[test]
    fn terminal_protocol_replies_are_not_refused_by_the_lock() {
        for reply in ["\x1b[24;80R", "\x1b[?1;2c", "\x1b[0n", "\x1b[I", "\x1b[O"] {
            assert!(
                auto_reply(reply),
                "fixture must be an automatic reply: {reply:?}"
            );
            assert_eq!(
                read_only_refusal(&ReadOnlySource::User, reply, None),
                None,
                "the terminal's own answer was refused: {reply:?}"
            );
        }
    }

    /// **Scrolling is reading.** A read-only tab exists so the user can WATCH
    /// it, and in an alt-screen TUI the wheel is forwarded to the program as a
    /// mouse report rather than scrolling xterm's own buffer — so a swallowed
    /// wheel means a tab one may watch but not scroll.
    ///
    /// The fixture table is duplicated verbatim in
    /// `src/lib/delegation.test.ts`: the courtesy gate and this enforcement
    /// point must agree about every one of these, or one of them refuses a
    /// scroll the other allowed.
    #[test]
    fn mouse_wheel_passes_the_lock_under_either_source() {
        let driven = ReadOnlySource::Driven {
            by: TabId::from_str("opencode"),
        };
        for wheel in [
            "\x1b[<64;10;5M",              // wheel up (SGR)
            "\x1b[<65;10;5M",              // wheel down
            "\x1b[<66;1;1M",               // wheel left
            "\x1b[<67;1;1M",               // wheel right
            "\x1b[<80;3;4M",               // ctrl+wheel up (64 + modifier 16)
            "\x1b[<68;3;4M",               // shift+wheel up (64 + modifier 4)
            "\x1b[M`!!",                   // wheel up, legacy X10 encoding
            "\x1b[Ma!!",                   // wheel down, legacy X10 encoding
            "\x1b[<64;1;1M\x1b[<64;1;1M",  // a fast scroll, coalesced
        ] {
            assert!(
                super::is_mouse_wheel(wheel),
                "fixture must be a wheel report: {wheel:?}"
            );
            for source in [&ReadOnlySource::User, &driven] {
                assert_eq!(
                    read_only_refusal(source, wheel, Some("api-work")),
                    None,
                    "a locked tab refused a scroll ({source:?}): {wheel:?}"
                );
            }
        }
    }

    /// **A click is input.** It activates whatever control is under it — a
    /// permission option, for one — which is the whole reason the lock exists.
    /// Drag and motion go with it: a drag is a held button.
    #[test]
    fn mouse_clicks_drags_and_pastes_are_still_refused() {
        for click in [
            "\x1b[<0;10;5M",   // left press
            "\x1b[<0;10;5m",   // left release
            "\x1b[<1;1;1M",    // middle press
            "\x1b[<2;1;1M",    // right press
            "\x1b[<32;5;5M",   // drag with button 0 held (motion bit)
            "\x1b[<35;5;5M",   // bare motion
            "\x1b[M !!",       // left press, legacy X10 encoding
            "\x1b[M#!!",       // release, legacy X10 encoding
            "\x1b[200~x\x1b[201~", // a bracketed paste
        ] {
            assert!(
                !super::is_mouse_wheel(click),
                "fixture must not be a wheel report: {click:?}"
            );
            assert!(
                read_only_refusal(&ReadOnlySource::User, click, None).is_some(),
                "mouse/paste input slipped through the lock: {click:?}"
            );
        }
    }

    /// **The exemption cannot carry a passenger.** A chunk that is a wheel
    /// report *plus* typed text is not a wheel report — otherwise the lock
    /// would be one concatenation away from open.
    #[test]
    fn a_wheel_report_with_anything_else_attached_is_refused() {
        for smuggled in [
            "\x1b[<64;1;1My",              // wheel then a keystroke
            "y\x1b[<64;1;1M",              // keystroke then wheel
            "\x1b[<64;1;1M\r",             // wheel then Enter
            "\x1b[<64;1;1M\x1b[<0;1;1M",   // wheel then a click
            "\x1b[<64;1;1",                // truncated: no terminator
            "\x1b[M`!",                    // truncated X10: two coord bytes
            "",
        ] {
            assert!(
                !super::is_mouse_wheel(smuggled),
                "fixture must not be a wheel report: {smuggled:?}"
            );
            assert!(
                read_only_refusal(&ReadOnlySource::User, smuggled, None).is_some(),
                "input smuggled past the lock behind a wheel report: {smuggled:?}"
            );
        }
    }

    /// The wheel exemption is `read_only_refusal`'s alone: the keystroke and
    /// submit bookkeeping still sees a wheel report exactly as it always did
    /// (not an automatic terminal response), so nothing about avatar state or
    /// echo suppression moved with this.
    #[test]
    fn the_wheel_exemption_did_not_change_the_automatic_reply_predicate() {
        for wheel in ["\x1b[<64;10;5M", "\x1b[M`!!"] {
            assert!(!auto_reply(wheel), "{wheel:?}");
        }
    }

    /// …and the exemption is narrow: an escape sequence a *person* produced
    /// (arrow keys, Esc, a bracketed paste) is still input, and still refused.
    #[test]
    fn keyboard_escape_sequences_are_still_refused() {
        for keys in ["\x1b[A", "\x1b", "\r", "y", "\x1b[200~pasted\x1b[201~"] {
            assert!(
                !auto_reply(keys),
                "fixture must not be an automatic reply: {keys:?}"
            );
            assert!(
                read_only_refusal(&ReadOnlySource::User, keys, None).is_some(),
                "user input slipped through the lock: {keys:?}"
            );
        }
    }

    /// **#48 (M-21): the manual buttons' refusal names the layer that is off.**
    ///
    /// The gate is unchanged and stays app-scoped — a worker-only override does
    /// not start the updater — so both cases below still refuse. What is asserted
    /// is the sentence: a user whose offload worker is screening every fetched
    /// page must not be told their injection detection is switched off, because
    /// that is a false statement about a running security layer and it is the one
    /// they would act on.
    #[test]
    fn the_updater_refusal_does_not_call_a_running_layer_off() {
        use crate::settings::injection::{Feature, Override};

        // Detection off everywhere: the plain sentence, and it is true.
        let mut off = Settings::default();
        off.set_l2_for_test(Feature::Detection, false);
        let plain = super::updates_allowed(&off).expect_err("the updater is off");
        let plain = plain.to_string();
        assert!(plain.contains("injection detection is switched off,"), "{plain}");
        assert!(!plain.contains("offload worker"), "nothing is running: {plain}");

        // M-21's state: off app-wide, ON for the offload worker. Still refused —
        // the scope semantics are deliberate — but for the reason that is true.
        let mut worker = off.clone();
        worker
            .set_worker_override_for_test(Feature::Detection, Override::On)
            .expect("detection has a worker row");
        assert!(
            super::updates_allowed(&worker).is_err(),
            "reporting honesty must not become a new capability"
        );
        let named = super::updates_allowed(&worker)
            .expect_err("still refused")
            .to_string();
        assert!(
            named.contains("still switched ON for the offload worker"),
            "the running layer must be named: {named}"
        );
        assert!(
            !named.contains("injection detection is switched off,"),
            "the false claim must not survive beside the true one: {named}"
        );
        // #48 F-35, M-21's residual: the THIRD state. The L1 master is off,
        // which resolves detection off with it — so "injection detection is
        // switched off" points at the wrong switch, the one the user can flip
        // with no effect until the master above it is back on. The frontend had
        // already made this distinction (`detectionUpdatesOffReason`); the two
        // surfaces now single-source from the same three cases.
        let mut master = Settings::default();
        master.set_master_for_test(false);
        assert!(
            super::updates_allowed(&master).is_err(),
            "reporting honesty must not become a new capability"
        );
        let l1 = super::updates_allowed(&master)
            .expect_err("still refused")
            .to_string();
        assert!(
            l1.contains("master switch"),
            "the switch that is actually off must be named: {l1}"
        );
        assert!(
            !l1.contains("injection detection is switched off,"),
            "the sentence that points at the wrong switch must not survive: {l1}"
        );
        assert!(
            !l1.contains("offload worker"),
            "an L1 off arms nothing anywhere: {l1}"
        );

        // All three refusals point at a section the sidebar has (F-18's tripwire
        // holds the pointer itself; this holds that the new sentences carry one).
        for r in [&plain, &named, &l1] {
            assert!(r.contains("Injection protection"), "{r}");
        }
    }

    /// `graph_ignore_pick`'s glob shaping: root-relative + `/`-anchored with
    /// forward slashes, trailing `/` for folders, longest root wins, and an
    /// out-of-root pick falls back to the absolute path. Built with `join` so
    /// the separators are the platform's, like a real picker result.
    #[test]
    fn to_ignore_glob_relativizes_and_anchors() {
        let root = std::env::temp_dir().join("ckg-pick-proj");
        let nested = root.join("nested");
        let roots = vec![root.clone(), nested.clone()];

        let file = root.join("src").join("a.rs");
        assert_eq!(super::to_ignore_glob(&file, false, &roots), "/src/a.rs");

        let dir = root.join("docs").join("gen");
        assert_eq!(super::to_ignore_glob(&dir, true, &roots), "/docs/gen/");

        // Under BOTH roots → the longer (nested) one wins.
        let in_nested = nested.join("x.md");
        assert_eq!(super::to_ignore_glob(&in_nested, false, &roots), "/x.md");

        // Outside every root → absolute fallback with forward slashes.
        let outside = std::env::temp_dir().join("ckg-pick-other").join("f.txt");
        assert_eq!(
            super::to_ignore_glob(&outside, false, &roots),
            outside.to_string_lossy().replace('\\', "/")
        );
    }

    #[test]
    fn focus_events_are_auto() {
        assert!(auto_reply("\x1b[I"));
        assert!(auto_reply("\x1b[O"));
    }

    #[test]
    fn da_and_cpr_replies_are_auto() {
        assert!(auto_reply("\x1b[?1;2c"));
        assert!(auto_reply("\x1b[?62;1;2;6;9;15;22c"));
        assert!(auto_reply("\x1b[10;20R"));
        assert!(auto_reply("\x1b[?1n"));
        assert!(auto_reply("\x1b[5n"));
    }

    #[test]
    fn arrow_keys_are_not_auto() {
        assert!(!auto_reply("\x1b[A"));
        assert!(!auto_reply("\x1b[B"));
        assert!(!auto_reply("\x1b[C"));
        assert!(!auto_reply("\x1b[D"));
        assert!(!auto_reply("\x1b[H"));
        assert!(!auto_reply("\x1b[1~"));
    }

    #[test]
    fn printable_and_control_are_not_auto() {
        assert!(!auto_reply("a"));
        assert!(!auto_reply("hello"));
        assert!(!auto_reply("\r"));
        assert!(!auto_reply("\x7f"));
        assert!(!auto_reply("\t"));
        assert!(!auto_reply("\x1b"));
        assert!(!auto_reply("\x1bf"));
    }

    // ── V17 Phase E — hideable_tool_calls recency window ────────────────────
    use super::count_hideable_tool_calls;
    use crate::activity::{ActivityEntry, ActivityKind};
    use crate::advisor::HIDEABLE_RECENCY_WINDOW_MS;

    fn hidden_call(ts_ms: u64) -> ActivityEntry {
        // `graph_cycles` is one of graph::LEAN_HIDDEN.
        ActivityEntry::new(
            ActivityKind::Graph,
            ts_ms,
            "root".to_string(),
            // An opaque source tag: this test is about the RECENCY window, and
            // `ActivityEntry::source` is a persisted free string (locked
            // decision 29). Asking the registry keeps it a real one without
            // hard-coding which harness happens to be first.
            crate::harness::DEFAULT_HARNESS.token().to_string(),
            "graph_cycles".to_string(),
            "target".to_string(),
            0,
            0,
            true,
            crate::activity::Attribution::Unattributed,
            None,
            None,
            None,
        )
    }

    #[test]
    fn hideable_call_inside_window_counts() {
        let now = 1_000_000_000_000;
        // One day inside the trailing window.
        let recent = now - (HIDEABLE_RECENCY_WINDOW_MS - 24 * 60 * 60 * 1000);
        let activity = vec![hidden_call(recent)];
        assert_eq!(
            count_hideable_tool_calls(&activity, now, HIDEABLE_RECENCY_WINDOW_MS),
            1
        );
    }

    #[test]
    fn hideable_call_outside_window_is_ignored() {
        let now = 1_000_000_000_000;
        // One day OLDER than the window edge — a cold-tail call from long ago
        // must not suppress the lean suggestion.
        let ancient = now - (HIDEABLE_RECENCY_WINDOW_MS + 24 * 60 * 60 * 1000);
        let activity = vec![hidden_call(ancient)];
        assert_eq!(
            count_hideable_tool_calls(&activity, now, HIDEABLE_RECENCY_WINDOW_MS),
            0
        );
    }

    #[test]
    fn non_hidden_tool_never_counts() {
        let now = 1_000_000_000_000;
        // A workhorse tool inside the window still doesn't count.
        let mut e = hidden_call(now - 1000);
        e.tool = "graph_find_symbol".to_string();
        assert_eq!(
            count_hideable_tool_calls(&[e], now, HIDEABLE_RECENCY_WINDOW_MS),
            0
        );
    }
}

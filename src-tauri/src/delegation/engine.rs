//! V39 Phase B — **the delegation engine**: preflight, write, wait, screen,
//! release.
//!
//! One `drive()` for both driver modes (locked decision 3), harness-agnostic by
//! construction (decision 16): the only harness-shaped things it touches are
//! `contract::gate(CAP_DELEGATION_WORKER)`, `harness::input_profile(id)` and
//! `chp::served(agent, tab, EV_ASSISTANT_TEXT)` — all three keyed by an id it
//! reads off the tab's own config and never spells.
//!
//! # The state machine
//!
//! ```text
//!   preflight ──refuse──▶ refused
//!       │
//!    (claim slot, engage auto lock, THEN write)
//!       ▼
//!     typed ──▶ waiting ◀──┐ prompt raised: relax lock, grant one extension
//!                 │        └── prompt resolved: re-engage lock
//!                 ├──▶ done | (done, no text)
//!                 ├──▶ timeout        (deadline; NO key is sent)
//!                 ├──▶ cancelled      (take-over; NO key is sent)
//!                 └──▶ worker_exited  (the subprocess went away)
//! ```
//!
//! # The three orderings that are load-bearing
//!
//! 1. **The lock engages BEFORE the write.** The "user types during the paste
//!    window" race is closed by ordering, not by timing — there is no window in
//!    which cImp is pasting and the keyboard is still live.
//! 2. **The submit timestamp is taken BEFORE the write.** A worker that answers
//!    fast must not have its completion filed under "earlier than our request".
//! 3. **The slot is claimed BEFORE anything is engaged.** A second driver
//!    losing the race must lose it without having touched the tab.
//!
//! # What this module can never do
//!
//! Send a key to stop a worker. Timeout, take-over and a vanished driver all
//! stop cImp *waiting*; the worker finishes visibly (locked decision 6). The
//! only bytes that reach a PTY from here are one profile's paste plus its
//! submit, and [`tests::the_only_bytes_written_are_the_profiles_paste_and_submit`]
//! is what keeps that true.

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::harness::contract::{self, CAP_DELEGATION_WORKER};
use crate::settings::{Settings, TabConfig};
use crate::state::TabId;

use super::{
    claim_checked, deadline_of, is_driver_gone, is_taken_over, is_worker_gone, mark_submitted,
    note_prompt, record_row, release, take_completion, transition, DelegationError, DelegationMode,
};

/// How often the wait loop re-reads the world.
///
/// A poll rather than a subscription, deliberately: the loop has to watch four
/// independent things (a completion, a prompt edge, a take-over flag and the
/// worker's process), only one of which is a stream. Four subscriptions plus a
/// deadline is four ways to miss an edge; one tick that re-reads state is one.
/// 200 ms is invisible against a turn measured in seconds-to-minutes.
const POLL: Duration = Duration::from_millis(200);

/// How much extra time ONE standing prompt buys the delegation.
///
/// Per prompt, on the rising edge — never per poll tick. See
/// [`super::note_prompt`] for why that distinction is the whole of locked
/// decision 5's compatibility with the failure-mode table: a per-tick grant
/// advances the deadline as fast as the clock, and a prompt nobody answers
/// would hang the driver forever instead of timing out and saying why.
///
/// Five minutes is the shape of the thing being waited for — a person noticing
/// a notification, switching to the tab and reading a permission prompt — not a
/// measurement. A worker that raises prompt after prompt is making real
/// progress between them, so each new one legitimately buys its own grant.
const PROMPT_GRACE: Duration = Duration::from_secs(300);

/// The Tauri event the frontend binds to for the *driven* glyph state, the
/// worker-tab banner and the status-bar chip.
///
/// A dedicated event rather than a `settings-changed` piggyback, and the reason
/// is that nothing about a delegation is in settings: `settings-changed` means
/// "the persisted configuration moved", and making it also mean "a transient,
/// never-persisted flight started" would force every existing listener to
/// re-read settings on an edge that changed none. Payload is
/// [`DelegationChanged`].
pub const EVENT_DELEGATION_CHANGED: &str = "delegation-changed";

/// The `delegation-changed` payload: **the whole in-flight set**, every time.
///
/// A snapshot rather than a delta because the set is tiny (one entry per driven
/// tab, and a worker is single-slot) and a delta stream has to be replayed from
/// a known start — a window that opens late, or a frontend that reloads, would
/// then paint a stale glyph until the next edge. A full snapshot is idempotent
/// and needs no sequence number.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DelegationChanged {
    /// `(worker tab id, what is driving it)`, sorted by tab id.
    pub in_flight: Vec<(String, super::InFlightView)>,
}

/// Publish the current in-flight set. Called on every transition, and cheap
/// enough to be called on the prompt edges too.
pub(crate) fn publish(app: &AppHandle) {
    let _ = app.emit(
        EVENT_DELEGATION_CHANGED,
        DelegationChanged {
            in_flight: super::statuses(),
        },
    );
}

/// One request to drive a worker tab.
#[derive(Clone, Debug)]
pub struct DriveRequest {
    /// The tab to drive.
    pub worker: TabId,
    /// The tab that asked. `None` ⇒ a headless consumer (a `claude -p`, a
    /// cron child), which is **refused**: the acyclic check needs a driver
    /// identity, and an unattributable delegation has no Events row worth
    /// writing.
    pub driver: Option<TabId>,
    pub mode: DelegationMode,
    /// The request, exactly as the caller wrote it. Typed **verbatim**.
    pub task: String,
    /// Optional extra context, appended to the task with a blank line and
    /// nothing else — see [`compose`].
    pub context: Option<String>,
    /// Caller's timeout override in seconds; `None` ⇒
    /// `delegation.default_timeout_s`.
    pub timeout_s: Option<u64>,
    /// **The one exception to "the task is typed verbatim"** (locked decision
    /// 2a), and it exists for exactly one caller: the Phase C facade.
    ///
    /// `offload_task` carries `schema` and `profile`, which a real backend
    /// honours through machinery cImp owns — a grammar on the final turn, a
    /// latch over the advertised tool defs. A worker tab has neither: cImp does
    /// not own its sampler and it does not own its tool surface. So the facade
    /// passes the *same instruction a real backend's worker would have been
    /// given* ([`crate::offload::agent::facade_format_note`]) and the engine
    /// appends it after the context — in one place, where a test can see it.
    ///
    /// `None` for the explicit tool: a user-directed hand-off adds nothing at
    /// all, and a test pins that.
    pub format_note: Option<String>,
}

/// A completed delegation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The worker's final assistant message, **after V32 screening**.
    pub text: String,
    /// The worker tab's display name — what the driver's result names as the
    /// source.
    pub worker: String,
    /// Flight time, submit → completion.
    pub duration_ms: u64,
    /// Whether the reply passed through the EXTERNAL screening boundary. Always
    /// `true` today; carried so the driver's result meta can say so rather than
    /// the caller assuming it.
    pub screened: bool,
}

/// The typed request: the task, then the context if there is one.
///
/// **No cImp-authored text is inserted** (locked decision 2a): no header, no
/// "Context:" label, no marker. The worker model must receive exactly what a
/// user would have typed, and the attribution lives client-side (the banner and
/// the local echo) plus in the Events row. A blank line is the separator
/// because it is what a person typing two paragraphs would produce, and it is
/// the only thing here that is not caller text.
///
/// `note` is [`DriveRequest::format_note`] — the ONE caller-independent
/// sentence cImp is allowed to add, and only the facade passes one. It goes
/// last, after the caller's context, because it is about the shape of the
/// answer rather than about the work.
fn compose(task: &str, context: Option<&str>, note: Option<&str>) -> String {
    let task = normalise_newlines(task);
    let context = context.map(normalise_newlines);
    let mut out = match context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!("{}\n\n{}", task.trim_end(), c),
        None => task,
    };
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        out = format!("{}\n\n{n}", out.trim_end());
    }
    out
}

/// `\r\n` → `\n`, and nothing else (V39 review HIGH-2).
///
/// The ONE transform applied to caller text, and it is meaning-preserving: a
/// Windows caller's paragraph break is a paragraph break. A BARE `\r` is not
/// normalised — it is refused by [`control_refusal`], because in a PTY a lone
/// CR is the submit key and guessing which of the two a caller meant is exactly
/// the guess that turns one request into two turns.
fn normalise_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// **The paste cannot be broken out of** (V39 review HIGH-2).
///
/// `InputProfile::paste_bytes` wraps the request in `ESC [ 200 ~` … `ESC [ 201
/// ~` and writes it to the PTY. Those markers are *in-band*: a task that itself
/// contains `ESC [ 201 ~` ends the paste early, and every byte after it is read
/// by the TUI as live keystrokes — `\r/exit\r` being the shape that matters.
/// The task is model-authored text (an `offload_task` instruction, a
/// `delegate_task_*` argument), so this is a request that reaches a shell, and
/// the bound has to be structural rather than a marker-shaped blocklist.
///
/// So: no `ESC`, and no C0/C1 control character other than `\n` and `\t`.
/// Everything an actual request needs is text, tabs and newlines.
///
/// **Refused, never sanitised.** Stripping the control bytes would send a
/// request the caller did not write, and truncating at the first one would send
/// half a question the worker would answer perfectly — the one failure mode a
/// worker cannot report. And it is refused at PREFLIGHT, before the slot claim,
/// so a hostile task leaves the worker exactly as it was.
fn control_refusal(typed: &str) -> Option<String> {
    let (at, c) = typed
        .char_indices()
        .find(|(_, c)| !matches!(c, '\n' | '\t') && c.is_control())?;
    let name = match c {
        '\u{1b}' => "ESC, U+001B".to_string(),
        '\r' => "CR, U+000D".to_string(),
        '\u{7f}' => "DEL, U+007F".to_string(),
        c => format!("U+{:04X}", c as u32),
    };
    Some(format!(
        "the task contains a control character ({name}) at byte {at} — a delegated request is \
         pasted into a TUI verbatim, so it may contain text, tabs and newlines only. Nothing was \
         typed and no tab was claimed"
    ))
}

/// The tool name the reply is screened under (locked decision 11).
///
/// `delegation__<worker tab id>`, and the shape is the point. `wrap_external_
/// result` screens a result iff `spotlight::is_external(name)`, which is
/// `toolclass::classify(name) == External`, which is **unknown ⇒ External**. A
/// bare tab id would therefore be a screening bypass by naming: a tab called
/// `read_file` classifies as LOCAL-CAPABILITY and its reply would sail through
/// unscreened. The prefix makes a collision with the class table structurally
/// impossible while still naming the worker in the envelope, and
/// [`tests::a_hostile_worker_tab_name_cannot_dodge_the_screen`] pins it against
/// every name the table knows.
fn screen_name(worker: &TabId) -> String {
    format!("delegation__{}", worker.as_str())
}

/// A named refusal. Every preflight exit goes through here, so none of them can
/// forget the reason.
fn refuse(reason: impl Into<String>) -> DelegationError {
    DelegationError::Refused(reason.into())
}

/// The harness id (CHP `agent` discriminator) of a configured AI tab, and its
/// config. `None` when the id names no AI tab.
fn ai_tab<'a>(
    settings: &'a Settings,
    tab: &TabId,
) -> Option<(&'static str, &'a crate::settings::AiToolTabConfig)> {
    match settings.find_tab(tab.as_str()) {
        Some(TabConfig::AiTool(c)) => Some((crate::tabs::tab_consumer(c), c)),
        _ => None,
    }
}

// ── the preflight checks that are properties of the WORKER ALONE ────────────
//
// Locked decision 12 lists ten conditions. Five of them ask nothing about the
// driver, the request or the moment — they ask whether this tab is a worker at
// all — and those five are exactly what Phase C's facade `is_ready` needs. They
// live here as named functions so [`drive`] and [`worker_ready`] cannot come to
// disagree about what a worker is: there is one rule per condition, written
// once, and both callers run it.
//
// The other five (a driver identity, a non-empty in-bounds task, the acyclic
// check, idleness, an empty input line) are properties of the REQUEST or the
// MOMENT and stay inline in `drive`. Idleness in particular is deliberately not
// here: for the router "the worker is mid-turn" is *no free slot*, not *not
// ready*, and folding it in would make a busy worker look permanently broken.

/// Preflight 1: the `delegation.worker` gate. A harness that is not gate-clean
/// is not a worker at all, which is why this is asked before anything about the
/// specific tabs.
fn gate_reason(settings: &Settings) -> Option<String> {
    let gate = contract::gate(CAP_DELEGATION_WORKER, settings);
    gate.blocked.then_some(gate.reason)
}

/// Preflight 4: the worker id names a configured AI tab. Answers its harness id.
fn worker_agent(settings: &Settings, worker: &TabId) -> Result<&'static str, String> {
    match ai_tab(settings, worker) {
        Some((agent, _)) => Ok(agent),
        None => Err(format!(
            "`{}` is not a configured AI tab, so it cannot be a delegation worker",
            worker.as_str()
        )),
    }
}

/// Preflight 5: a live process behind the tab. Takes the two facts rather than
/// reading them, because `drive` already holds both and re-reading them under a
/// second lock is how two answers to one question start.
fn worker_process(alive: bool, exited: bool, worker_name: &str) -> Result<(), String> {
    if !alive || exited {
        return Err(format!(
            "worker tab `{worker_name}` has no running process — start it and try again"
        ));
    }
    Ok(())
}

/// Preflight 6: the worker's harness declares an input profile. `None` is the
/// fail-closed half of decision 16 — a harness cImp cannot type into is not a
/// worker.
fn worker_profile(
    agent: &str,
    worker_name: &str,
) -> Result<crate::harness::InputProfile, String> {
    crate::harness::input_profile(agent).ok_or_else(|| {
        format!(
            "worker tab `{worker_name}` runs a harness with no input profile, so cImp does not              know how to submit a turn to it"
        )
    })
}

/// Preflight 11: a completion signal exists — either the harness pushes CHP
/// `assistant_text` for THIS tab, or it has a fallback reader attached. Without
/// one the engine would type into a tab it cannot read back from, which is the
/// silent-swallow decision 12 exists to prevent.
fn worker_completion_source(agent: &str, worker: &TabId, worker_name: &str) -> Result<(), String> {
    let pushed = crate::harness::chp::served(
        agent,
        worker.as_str(),
        crate::harness::chp::EV_ASSISTANT_TEXT,
    );
    let reader = crate::harness::reader::has_live_reader(worker);
    if !pushed && !reader {
        return Err(format!(
            "worker tab `{worker_name}` has no way to report the end of a turn (its harness pushes              no assistant text for this tab and no fallback reader is attached), so cImp could not              read the answer back — restart the tab and try again"
        ));
    }
    Ok(())
}

/// Preflight 9 + 10 as one rule: is the worker FREE right now?
///
/// `None` when it is; otherwise the named reason, in the order `drive` asks
/// them — a turn in flight, then a standing prompt, then text the user has
/// typed and not sent. One function because these three are one question asked
/// by two callers ([`drive`], which refuses, and [`worker_busy`], which the
/// router reads as "no free slot"), and a rule written twice is a rule that
/// answers differently in two places.
/// Takes the three FACTS rather than the state mirror: they are what the rule
/// is about, so both callers can hand it their own reading of the world — and a
/// test can assert on it without a running state manager.
fn busy_reason(
    output_running: bool,
    awaiting_prompt: bool,
    pending: i32,
    worker_name: &str,
) -> Option<String> {
    if output_running {
        return Some(format!(
            "worker tab `{worker_name}` is mid-turn — a request typed now would land in the middle \
             of someone else's turn"
        ));
    }
    if awaiting_prompt {
        return Some(format!(
            "worker tab `{worker_name}` is waiting for an answer to a prompt — answer it, then \
             delegate"
        ));
    }
    if pending > 0 {
        return Some(format!(
            "worker tab `{worker_name}` has {pending} characters typed and not sent — clear the \
             input line first"
        ));
    }
    None
}

/// The user's own activity on a worker tab, as the router's **free slot**
/// question (V39 Phase C).
///
/// Deliberately NOT part of [`worker_ready`]: a tab whose user is mid-turn is
/// not broken, it is busy, and the two are different answers. Readiness that
/// dropped on every keystroke would take a facade out of the pool and back in;
/// busy-ness makes the router prefer a backend that can start now and fall back
/// to this one only when there is nothing else — which is exactly what it does
/// with a full llama-server.
pub async fn worker_busy(app: &AppHandle, worker: &TabId) -> bool {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return true;
    };
    let flags = state.tab_activity.flags(worker);
    let pending = {
        let map = state
            .input_lengths
            .read()
            .unwrap_or_else(|e| e.into_inner());
        map.get(worker)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    };
    busy_reason(
        flags.output_running,
        flags.awaiting_prompt(),
        pending,
        worker.as_str(),
    )
    .is_some()
}

/// **Is this tab a worker right now?** — the facade backend's readiness
/// (V39 Phase C, locked decision 3: "`is_ready` = preflight conditions minus
/// idleness").
///
/// The five worker-only checks above, in `drive`'s own order, and nothing else:
/// no driver (there is none yet — the router is asking about capacity, not
/// about a call), no idleness (that is the free-slot question, and
/// `in_flight` answers it), no slot claim (asking must never take one).
///
/// `Err` carries the reason, unchanged from the one `drive` would refuse with,
/// so a backend that is "down" in the pool and a delegation that is refused at
/// preflight tell the user the same story.
pub async fn worker_ready(app: &AppHandle, worker: &TabId) -> Result<(), String> {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return Err("cImp's tab layer is not running, so no tab can be driven".to_string());
    };
    let settings = state.settings.current();
    if let Some(reason) = gate_reason(&settings) {
        return Err(reason);
    }
    let agent = worker_agent(&settings, worker)?;
    let worker_name = {
        let registry = state.tabs.lock().await;
        registry
            .name_of(worker)
            .unwrap_or_else(|| worker.as_str().to_string())
    };
    let alive = {
        let registry = state.tabs.lock().await;
        registry.is_started(worker).await
    };
    worker_process(alive, state.tab_activity.flags(worker).exited, &worker_name)?;
    worker_profile(agent, &worker_name)?;
    worker_completion_source(agent, worker, &worker_name)
}

/// **Drive one worker tab and return its answer.**
///
/// Every failure is a named [`DelegationError`]; every outcome mints exactly
/// one terminal `delegation` Events row, plus one `start` row for a delegation
/// that actually started. A refusal mints only `refused` — nothing was locked,
/// nothing was typed, so a `start` row would be a lie.
pub async fn drive(app: &AppHandle, req: DriveRequest) -> Result<Reply, DelegationError> {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return Err(refuse(
            "cImp's tab layer is not running, so no tab can be driven",
        ));
    };
    let settings = state.settings.current();

    // ── preflight (locked decision 12), in order, each failure named ────────
    //
    // Nothing below this block has a side effect until the slot is claimed, so
    // a refusal at ANY of these steps leaves the worker exactly as it was.

    // 1. The gate. A harness that is not gate-clean is not a worker at all, so
    //    this is asked before anything about the specific tabs.
    if let Some(reason) = gate_reason(&settings) {
        let e = refuse(reason);
        record_refusal(&req, &settings, "unknown", &e);
        return Err(e);
    }

    // 2. A driver identity. The cycle check needs it and the Events row is
    //    worthless without it.
    let Some(driver) = req.driver.clone() else {
        let e = refuse(
            "no calling tab: delegation is refused for a headless consumer, because the acyclic \
             check and the audit row both need to know who asked",
        );
        record_refusal(&req, &settings, "unknown", &e);
        return Err(e);
    };
    let Some((driver_agent, _)) = ai_tab(&settings, &driver) else {
        let e = refuse(format!(
            "the calling tab `{}` is not a configured AI tab",
            driver.as_str()
        ));
        record_refusal(&req, &settings, "unknown", &e);
        return Err(e);
    };
    let driver_name = {
        let registry = state.tabs.lock().await;
        registry
            .name_of(&driver)
            .unwrap_or_else(|| driver.as_str().to_string())
    };

    // From here on every refusal is attributable, so they all carry the driver.
    let deny = |reason: String| -> DelegationError {
        let e = refuse(reason);
        record_refusal(&req, &settings, driver_agent, &e);
        e
    };

    // 3. Not itself.
    if driver == req.worker {
        return Err(deny(format!(
            "a tab cannot delegate to itself (`{}`)",
            driver.as_str()
        )));
    }

    // 4. The worker is a configured AI tab…
    let worker_agent = match worker_agent(&settings, &req.worker) {
        Ok(a) => a,
        Err(reason) => return Err(deny(reason)),
    };
    let worker_name = {
        let registry = state.tabs.lock().await;
        registry
            .name_of(&req.worker)
            .unwrap_or_else(|| req.worker.as_str().to_string())
    };

    // 5. …with a live process. Checked here rather than discovered by a failed
    //    write: a refusal that surfaced from the write has already engaged the
    //    lock and minted a `start` row for a delegation that never began.
    let alive = {
        let registry = state.tabs.lock().await;
        registry.is_started(&req.worker).await
    };
    let flags = state.tab_activity.flags(&req.worker);
    if let Err(reason) = worker_process(alive, flags.exited, &worker_name) {
        return Err(deny(reason));
    }

    // 6. Its harness declares an input profile. `None` here is the fail-closed
    //    half of decision 16: a harness cImp cannot type into is not a worker.
    let profile = match worker_profile(worker_agent, &worker_name) {
        Ok(p) => p,
        Err(reason) => return Err(deny(reason)),
    };

    // 7. A substantive request that fits the profile's paste bound. Refused,
    //    never truncated — half a request is the one failure a worker cannot
    //    report, because it would answer the truncated question perfectly.
    let typed = compose(
        &req.task,
        req.context.as_deref(),
        req.format_note.as_deref(),
    );
    if typed.trim().is_empty() {
        return Err(deny("the task is empty".to_string()));
    }
    if !profile.fits(&typed) {
        return Err(deny(format!(
            "the task is {} bytes, over this harness's {}-byte paste limit — split it rather than \
             sending a truncated request",
            typed.len(),
            profile.max_paste_bytes
        )));
    }
    // 7b. …and one the paste cannot be broken out of (V39 review HIGH-2). The
    //     bracketed-paste markers are in-band, so a task carrying `ESC [ 201 ~`
    //     would end the paste and leave the rest to land as live keystrokes.
    if let Some(reason) = control_refusal(&typed) {
        return Err(deny(reason));
    }

    // 8. Acyclic, and within `delegation.max_depth` (locked decision 9) —
    //    **asked below, inside the claim** (V39 review M-8). Both directions
    //    and the depth bound are properties of the registry, and asking them
    //    here, under their own lock, left the whole of the remaining preflight
    //    between the answer and the claim that depends on it: A→B and B→A
    //    racing both passed, then claimed DIFFERENT workers, and the cycle
    //    existed. `claim_checked` asks and claims under one lock, which also
    //    makes the claim the LAST preflight step — nothing can refuse after it.

    // 9 + 10. Free: no output burst in progress, no prompt standing, and
    //     nothing the user has half-typed and not sent. (Pasting into a composer
    //     that already holds text would send the two concatenated, under the
    //     user's name.) One rule, shared with the router's free-slot question —
    //     see `busy_reason`.
    let pending = {
        let map = state
            .input_lengths
            .read()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&req.worker)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    };
    if let Some(reason) = busy_reason(
        flags.output_running,
        flags.awaiting_prompt(),
        pending,
        &worker_name,
    ) {
        return Err(deny(reason));
    }

    // 11. A completion signal exists. Either the harness pushes CHP
    //     `assistant_text` for THIS tab, or it has a fallback reader attached.
    //     Without one the engine would type into a tab it cannot read back
    //     from, which is the silent-swallow decision 12 exists to prevent.
    if let Err(reason) = worker_completion_source(worker_agent, &req.worker, &worker_name) {
        return Err(deny(reason));
    }

    // ── the slot, then the lock, then the write ─────────────────────────────

    let now = crate::activity::now_ms();
    let timeout_s = req
        .timeout_s
        .filter(|s| *s > 0)
        .unwrap_or(settings.delegation.default_timeout_s)
        .max(1);
    let deadline = now.saturating_add(timeout_s.saturating_mul(1000));
    // The acyclic check, the depth bound and the slot, atomically (M-8). Every
    // refusal it can answer is worded exactly as the four separate checks were.
    if let Err(refusal) = claim_checked(
        &req.worker,
        &worker_name,
        driver.clone(),
        driver_name.clone(),
        driver_agent.to_string(),
        req.mode,
        now,
        deadline,
        settings.delegation.max_depth,
    ) {
        return Err(deny(refusal));
    }

    // Everything past this point owns the slot and must release it on EVERY
    // path, so the body is a closure-shaped block whose result is handled once.
    let auto_lock = settings.delegation.auto_read_only;
    if auto_lock {
        // BEFORE the write (locked decision 12 + the "user types during the
        // paste window" failure mode): the window is closed by ordering.
        state.read_only.set_driven(&req.worker, Some(driver.clone()));
    }
    publish(app);
    record_row(
        transition::START,
        &worker_name,
        None,
        driver_agent,
        Some(driver.as_str()),
        true,
        0,
        typed.clone(),
        String::new(),
    );

    let outcome = run_flight(
        app,
        &state,
        &req.worker,
        &worker_name,
        &typed,
        profile,
        auto_lock,
    )
    .await;

    // ── release, screen, record ─────────────────────────────────────────────

    if auto_lock {
        // Clears ONLY the engine's lock: a `User` lock the tab already carried
        // survives (Phase A keeps the two sources side by side for exactly
        // this).
        state.read_only.set_driven(&req.worker, None);
    }
    let started_ms = now;
    release(&req.worker);
    publish(app);

    match outcome {
        Ok((raw, submit_ms)) => {
            let duration_ms = crate::activity::now_ms().saturating_sub(submit_ms);
            // Locked decision 13 — empty is not absent. A completed turn whose
            // text is whitespace or tool scaffolding only is an ERROR, never an
            // empty success: a driver handed "" would report the task done.
            if !substantive(&raw) {
                let e = DelegationError::NoText(format!(
                    "worker produced no text: tab `{worker_name}` finished its turn without a \
                     substantive final message"
                ));
                record_row(
                    e.transition(),
                    &worker_name,
                    Some(e.reason()),
                    driver_agent,
                    Some(driver.as_str()),
                    false,
                    duration_ms,
                    typed,
                    // V39 review M-6: the RAW text, not an empty string. This
                    // row is the only record that the turn happened at all, and
                    // "the worker produced nothing substantive" is a verdict
                    // about text — a verdict whose evidence was discarded is
                    // one nobody can check. Unscreened by construction:
                    // screening is for text entering a MODEL's context, and
                    // this text reaches no model.
                    raw,
                );
                return Err(e);
            }
            // Locked decision 11: the worker's text is model-generated text
            // entering another model's context. It crosses the same boundary
            // every external tool result crosses — it is not trusted because it
            // came from a sibling tab.
            let text = screen(&settings, driver_agent, &driver, &req.worker, raw).await;
            record_row(
                transition::DONE,
                &worker_name,
                None,
                driver_agent,
                Some(driver.as_str()),
                true,
                duration_ms,
                typed,
                text.clone(),
            );
            Ok(Reply {
                text,
                worker: worker_name,
                duration_ms,
                screened: true,
            })
        }
        Err(e) => {
            let duration_ms = crate::activity::now_ms().saturating_sub(started_ms);
            record_row(
                e.transition(),
                &worker_name,
                Some(e.reason()),
                driver_agent,
                Some(driver.as_str()),
                false,
                duration_ms,
                typed,
                String::new(),
            );
            Err(e)
        }
    }
}

/// The write + wait half, from an owned slot to a terminal outcome.
///
/// Returns the raw completion text and the submit timestamp, so the caller can
/// report flight time measured from the request rather than from the claim.
#[allow(clippy::too_many_arguments)]
async fn run_flight(
    app: &AppHandle,
    state: &crate::ipc::AppState,
    worker: &TabId,
    worker_name: &str,
    typed: &str,
    profile: crate::harness::InputProfile,
    auto_lock: bool,
) -> Result<(String, u64), DelegationError> {
    // The correlation floor is taken BEFORE the write: a worker that answers
    // fast must not have its completion filed as "earlier than our request".
    let submit_ms = crate::activity::now_ms();
    mark_submitted(worker, submit_ms);

    // The ONE input pipeline (the V39 cross-module invariant): the same
    // `write_through_pipeline` `pty_write` runs — TTS-marker registration,
    // `note_typed_input`, the `UserSubmit` signal — reached without the
    // read-only check, which is the only thing the engine bypasses and only
    // because it holds the lock itself.
    let paste = String::from_utf8_lossy(&profile.paste_bytes(typed)).into_owned();
    // V39 review L-1: the paste is NOT the submit, whatever its bytes look
    // like. A multi-line request is one bracketed paste full of newlines, and
    // letting the pipeline infer the submit from them raised `UserSubmit` one
    // write early — clearing the worker's prompt mirror and zeroing its input
    // counter for a turn that had not started.
    if let Err(e) = crate::ipc::commands::write_through_pipeline(
        state,
        worker,
        paste,
        crate::ipc::commands::Submit::No,
    )
    .await
    {
        return Err(DelegationError::WorkerExited(format!(
            "could not type into worker tab `{worker_name}`: {e}"
        )));
    }
    // The settle. Not cosmetic — see the harness input profiles: a submit that
    // arrives inside the TUI's own paste debounce is evaluated against a buffer
    // it has not finished ingesting.
    tokio::time::sleep(Duration::from_millis(profile.settle_ms)).await;
    let submit = String::from_utf8_lossy(profile.submit).into_owned();
    if let Err(e) = crate::ipc::commands::write_through_pipeline(
        state,
        worker,
        submit,
        crate::ipc::commands::Submit::Yes,
    )
    .await
    {
        return Err(DelegationError::WorkerExited(format!(
            "could not submit the turn on worker tab `{worker_name}`: {e}"
        )));
    }

    let mut relaxed = false;
    loop {
        tokio::time::sleep(POLL).await;

        // Take-over first: it is the user's explicit instruction, and it must
        // win over a completion that landed in the same tick.
        if is_taken_over(worker) {
            if relaxed && auto_lock {
                // Leave nothing relaxed behind us; `drive`'s `set_driven(None)`
                // clears it too, but the two must agree even if this path is
                // reached first.
                state.read_only.set_prompt_relaxed(worker, false);
            }
            // Decision 6's own words, and the ONE string this outcome has: the
            // driver reads it as its tool result and the `takeover` row
            // records it as its reason, so `target` reads
            // "<worker> — cancelled: user took over".
            let _ = worker_name;
            return Err(DelegationError::Cancelled(
                "cancelled: user took over".to_string(),
            ));
        }

        // V39 review L-7: the DRIVER went away — its `offload_task` client
        // disconnected. Stop waiting: the reply has nowhere to go, and the
        // alternative is holding this worker's slot and the global offload
        // permit behind it for the rest of the deadline (up to ten minutes on
        // the default). Read here, beside the take-over, because it is the same
        // kind of fact: somebody stopped needing this turn. NO key is sent —
        // the worker finishes visibly, exactly as the design's "driver tab
        // closes while waiting" row says.
        if is_driver_gone(worker) {
            if relaxed && auto_lock {
                state.read_only.set_prompt_relaxed(worker, false);
            }
            return Err(DelegationError::DriverGone(format!(
                "the caller went away while tab `{worker_name}` was working — cImp stopped \
                 waiting and sent it nothing; the turn finishes in its own tab"
            )));
        }

        // TWO ways a worker can go away, and they need two checks: the
        // subprocess exiting (the mirror sees `SubprocessExited`) and the TAB
        // being closed (which drops the mirror row entirely, so the mirror's
        // answer becomes indistinguishable from a healthy idle tab — see
        // `note_worker_gone`).
        let flags = state.tab_activity.flags(worker);
        if flags.exited {
            return Err(DelegationError::WorkerExited(format!(
                "worker tab `{worker_name}` exited while the task was running"
            )));
        }
        if is_worker_gone(worker) {
            return Err(DelegationError::WorkerExited(format!(
                "worker tab `{worker_name}` was closed while the task was running"
            )));
        }

        // Locked decision 5: a standing prompt relaxes the lock (answering a
        // prompt the worker addressed to the user is the only way this
        // completes) and extends the deadline for as long as it stands. The
        // extension is applied per tick, so a prompt nobody answers still runs
        // out — it buys time, it does not stop the clock.
        let awaiting = flags.awaiting_prompt();
        let (_, changed) = note_prompt(worker, awaiting, PROMPT_GRACE.as_millis() as u64);
        if changed && auto_lock {
            // V39 review M-5: a DEDICATED relaxation flag, not "clear the
            // engine's lock for the duration". Clearing it let
            // `ReadOnlyEntry::source` fall back to a `User` lock the tab was
            // already carrying — so on a user-read-only tab the prompt only the
            // user can answer could not be answered, and the flight ran to its
            // deadline reporting "worker awaiting permission". It also dropped
            // the driver identity the banner and Take over read.
            state.read_only.set_prompt_relaxed(worker, awaiting);
            // The signal has a consumer: the lock re-engages on the falling
            // edge. A relaxation nobody re-engages would be a delegation that
            // silently gave the keyboard back for the rest of its run.
            relaxed = awaiting;
        }
        if changed {
            publish(app);
        }

        if let Some(text) = take_completion(worker) {
            return Ok((text, submit_ms));
        }

        let deadline = deadline_of(worker).unwrap_or(0);
        if crate::activity::now_ms() >= deadline {
            // NO key is sent. The worker finishes what it is doing, visibly.
            let why = if awaiting {
                " (worker awaiting permission)"
            } else {
                ""
            };
            return Err(DelegationError::Timeout(format!(
                "worker tab `{worker_name}` did not finish in time{why} — it is still running; \
                 cImp stopped waiting and sent it nothing"
            )));
        }
    }
}

/// Whether a completed turn's text is substantive (locked decision 13).
///
/// Whitespace-only is the obvious half. The other half used to be "any reply
/// that is nothing but fenced blocks", and V39 review M-6 is what that costs:
/// **a fenced block is very often the whole answer.** "Write the regex" /
/// "produce the JSON" / "show me the diff" are answered by one code block and
/// nothing else, and such a reply was reported to the driver as
/// `worker produced no text` — a delegation that did the work, said so, and
/// was told it had failed.
///
/// So a fence is substantive unless it is recognisable SCAFFOLDING, and the
/// list of shapes checked is deliberately short (see [`is_scaffold_block`]):
/// an empty block, or an empty JSON payload. Anything else — code, data, prose
/// — is an answer.
///
/// An UNTERMINATED fence is substantive whenever it holds non-whitespace: a
/// truncated block is a partial answer, and the driver can see that it is.
fn substantive(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let mut outside = String::new();
    // `Some((info string, body))` while a fence is open.
    let mut open: Option<(String, String)> = None;
    for line in t.lines() {
        let is_fence = line.trim_start().starts_with("```");
        if let Some((_, body)) = open.as_mut() {
            if !is_fence {
                body.push_str(line);
                body.push('\n');
                continue;
            }
            let (info, body) = open.take().expect("open");
            if !is_scaffold_block(&info, &body) {
                return true;
            }
            continue;
        }
        if is_fence {
            let info = line.trim_start().trim_start_matches('`').trim().to_string();
            open = Some((info, String::new()));
            continue;
        }
        outside.push_str(line);
    }
    // An unterminated block: what is in it still counts.
    if let Some((info, body)) = open {
        if !is_scaffold_block(&info, &body) {
            return true;
        }
    }
    !outside.trim().is_empty()
}

/// The scaffold shapes [`substantive`] recognises. **Short on purpose**: every
/// shape here is a reply cImp will call empty, so a wrong entry silently turns
/// a real answer into `worker produced no text`.
///
/// 1. an empty block (nothing but whitespace);
/// 2. an empty JSON payload — `{}`, `[]`, `null` — which is what a worker that
///    was asked for JSON and then produced nothing emits.
///
/// **Both are shapes, not vocabulary, and that is a layering constraint as
/// well as a preference.** A list of info strings naming tool-protocol
/// sections (`tool_use`, `tool_result`, …) would be harness-owned vocabulary
/// in an L4 module — which `no_harness_literals_outside_harness` forbids, and
/// rightly: a fenced block a harness happens to label is still a block this
/// module cannot read. A worker whose whole final message is one such block
/// therefore returns it, which is the safe direction: the driver gets text it
/// can judge, instead of a task that did real work being reported as empty.
///
/// `info` is unused today and kept in the signature because it is what a
/// future shape would key on, and because it documents that the info string
/// was considered rather than forgotten.
fn is_scaffold_block(_info: &str, body: &str) -> bool {
    let body = body.trim();
    body.is_empty() || matches!(body, "{}" | "[]" | "null")
}

/// Run the worker's reply through the V32 EXTERNAL boundary (locked decision
/// 11).
async fn screen(
    settings: &Settings,
    driver_agent: &'static str,
    driver: &TabId,
    worker: &TabId,
    text: String,
) -> String {
    let inj_scope =
        crate::settings::injection::Scope::for_tab(driver_agent, Some(driver.as_str()));
    let scope_label = format!("{driver_agent}:{}", driver.as_str());
    // The driver's identity-less ledger. A delegation reply is not a proxied
    // MCP call, so there is no tab `Budget` to hang the claim bits on; this is
    // the same bounded, per-agent ledger every other scope-less screen uses, so
    // a driver in a refusal loop still costs log2(n) rows rather than n.
    let audit = crate::offload::outbound::UnscopedAudit::for_agent(driver_agent);
    crate::offload::detection::wrap_external_result(
        &screen_name(worker),
        text,
        crate::offload::detection::ResultCtx {
            consumer: driver_agent,
            scope: &scope_label,
            root: String::new(),
            url: None,
            host: None,
            cfg: crate::offload::detection::Config::from_settings(settings, inj_scope),
            spotlight: crate::settings::injection::effective(
                crate::settings::injection::Feature::Spotlighting,
                inj_scope,
                settings,
            ),
            audit: &audit,
            // The driver receives the whole reply — nothing downstream of this
            // truncates it, so the unscreened notice is load-bearing here.
            delivered_bytes: usize::MAX,
        },
    )
    .await
}

/// Mint the `refused` row for a preflight exit.
///
/// Separate from the terminal-row path in `drive` because a refusal has no
/// flight: `ms` is 0, there is no `start` row to pair with, and the worker name
/// may not be resolvable (the tab may not exist, which is often the refusal).
fn record_refusal(
    req: &DriveRequest,
    settings: &Settings,
    driver_agent: &str,
    e: &DelegationError,
) {
    let worker_name = match settings.find_tab(req.worker.as_str()) {
        Some(TabConfig::AiTool(c)) => c.name.clone(),
        _ => req.worker.as_str().to_string(),
    };
    record_row(
        e.transition(),
        &worker_name,
        Some(e.reason()),
        driver_agent,
        req.driver.as_ref().map(|d| d.as_str()),
        false,
        0,
        req.task.clone(),
        String::new(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Busy is not broken** (V39 Phase C).
    ///
    /// The same three facts answer two questions — `drive` refuses with the
    /// reason, and the facade backend reports "no free slot" — so they are one
    /// function. What this pins is that each fact speaks, that the order is the
    /// one `drive` asks in (the most specific thing the user can act on first),
    /// and that a free tab says nothing at all.
    #[test]
    fn a_busy_worker_names_the_one_thing_the_user_can_act_on() {
        assert_eq!(busy_reason(false, false, 0, "api-work"), None);

        let mid = busy_reason(true, false, 0, "api-work").expect("mid-turn is busy");
        assert!(mid.contains("api-work") && mid.contains("mid-turn"));

        let prompt = busy_reason(false, true, 0, "api-work").expect("a standing prompt is busy");
        assert!(prompt.contains("waiting for an answer to a prompt"));

        let typed = busy_reason(false, false, 12, "api-work").expect("typed text is busy");
        assert!(
            typed.contains("12 characters typed and not sent"),
            "the reason must say how much: {typed}"
        );

        // Order: a tab that is mid-turn AND has text typed reports the turn,
        // because that is the one the user cannot simply clear.
        assert_eq!(busy_reason(true, false, 12, "api-work"), Some(mid));
    }

    /// **The task is typed verbatim** (locked decision 2a/10). No header, no
    /// marker, nothing a worker model could read as provenance — the
    /// attribution is client-side and in the Events row, and nowhere else.
    #[test]
    fn the_composed_request_is_the_callers_text_and_nothing_else() {
        assert_eq!(
            compose("summarise latch.ts", None, None),
            "summarise latch.ts"
        );
        assert_eq!(
            compose("summarise latch.ts", Some("   "), None),
            "summarise latch.ts",
            "a blank context is not a context"
        );
        assert_eq!(
            compose("summarise latch.ts", None, Some("  ")),
            "summarise latch.ts",
            "a blank note is not a note"
        );
        let both = compose("do the thing", Some("src/lib/latch.ts"), None);
        assert_eq!(both, "do the thing\n\nsrc/lib/latch.ts");
        for marker in ["delegated", "cImp", "via", "[", "Context:", "Task:"] {
            assert!(
                !both.contains(marker),
                "the typed request must carry no cImp-authored text, found {marker:?}"
            );
        }
    }

    /// **V39 Phase C: the format note goes LAST, and only when there is one.**
    ///
    /// The facade's one licence to add text (`DriveRequest::format_note`) — and
    /// the order is the contract: the caller's task, then the caller's context,
    /// then cImp's sentence about the answer. A note that landed between the
    /// task and its context would split a request in half.
    #[test]
    fn the_format_note_is_appended_after_the_context() {
        assert_eq!(
            compose("do it", Some("ctx"), Some("answer in JSON")),
            "do it\n\nctx\n\nanswer in JSON"
        );
        assert_eq!(
            compose("do it", None, Some("answer in JSON")),
            "do it\n\nanswer in JSON",
            "no context: the note follows the task directly"
        );
    }

    /// **The paste is not a submit; the submit is** (V39 review L-1).
    ///
    /// `write_through_pipeline` used to infer it from the bytes, and the
    /// engine's paste is full of newlines — a multi-line request is one
    /// bracketed paste — so `UserSubmit` fired one write early, clearing the
    /// worker's prompt mirror and zeroing its input counter for a turn that had
    /// not started. Asserted on the source because the pipeline needs a running
    /// `AppState`, and what must hold is a property of the CALL SITES: there
    /// are exactly two, and they disagree about this argument.
    #[test]
    fn the_paste_is_written_as_not_a_submit_and_the_submit_as_one() {
        let src = include_str!("engine.rs").replace('\r', "");
        let code = crate::rustsrc::code_of("delegation/engine.rs", &src);
        let mut body = String::new();
        let mut at = 0usize;
        for (start, end) in crate::rustsrc::test_regions(&code) {
            let (start, end) = (start.min(src.len()), end.min(src.len()));
            if start > at {
                body.push_str(&src[at..start]);
            }
            at = end.max(at);
        }
        if at < src.len() {
            body.push_str(&src[at..]);
        }
        let calls: Vec<&str> = body
            .split("write_through_pipeline(")
            .skip(1)
            .map(|rest| &rest[..rest.len().min(220)])
            .collect();
        assert_eq!(calls.len(), 2, "the paste and the submit, and nothing else");
        assert!(
            calls[0].contains("Submit::No"),
            "the paste must not be written as a submit: {}",
            calls[0]
        );
        assert!(
            calls[1].contains("Submit::Yes"),
            "the submit must be: {}",
            calls[1]
        );
    }

    /// **A restarted worker is a worker again** (V39 review HIGH-3).
    ///
    /// `TabActivity::exited` is latched — a dead process is not something the
    /// mirror can un-observe — so the whole question is who clears it. Before
    /// this fix, only `TabAdded` did, which a restart into an existing tab
    /// never sends: the tab was refused "has no running process" forever.
    #[test]
    fn a_restarted_worker_stops_being_refused_as_exited() {
        use crate::state::{StateSignal, TabActivity, TabId};
        let mirror = TabActivity::default();
        let tab = TabId::from_str("ai-restart-probe");
        mirror.note_signal(&StateSignal::ClaudeOutputStarted { tab: tab.clone() });
        mirror.note_signal(&StateSignal::SubprocessExited {
            tab: tab.clone(),
            code: Some(1),
        });
        let dead = mirror.flags(&tab);
        assert!(dead.exited);
        assert!(
            worker_process(true, dead.exited, "api-work").is_err(),
            "an exited worker is refused, which is right while it IS exited"
        );

        mirror.note_signal(&StateSignal::ShellRestarted { tab: tab.clone() });
        let alive = mirror.flags(&tab);
        assert_eq!(
            alive,
            Default::default(),
            "a restart re-seeds the whole row, not just `exited`"
        );
        assert!(
            worker_process(true, alive.exited, "api-work").is_ok(),
            "a restarted worker must be drivable again"
        );
    }

    /// **A task cannot break out of the paste** (V39 review HIGH-2).
    ///
    /// The markers are in-band, so this is the whole boundary: refuse at
    /// preflight, name the character, and never sanitise. The fixture that
    /// matters most is the first one — with it accepted, everything after the
    /// end marker reaches the TUI as live keystrokes.
    #[test]
    fn a_task_that_could_end_the_paste_early_is_refused_by_name() {
        for (label, hostile) in [
            ("the end marker itself", "summarise this\u{1b}[201~\r/exit\r"),
            ("a bare ESC", "look at \u{1b}OA the file"),
            ("the start marker", "\u{1b}[200~nested"),
            ("Ctrl-C", "stop \u{3} now"),
            ("DEL", "oops\u{7f}"),
            ("a bare CR", "line one\rline two"),
            ("a C1 control", "text \u{9b}[201~"),
            ("NUL", "a\u{0}b"),
        ] {
            let reason = control_refusal(hostile)
                .unwrap_or_else(|| panic!("{label} must be refused: {hostile:?}"));
            assert!(
                reason.contains("control character") && reason.contains("U+"),
                "{label}: the refusal must name the character: {reason}"
            );
        }
        // Everything a real request is made of passes.
        for fine in [
            "summarise src/lib/latch.ts",
            "line one\nline two\n\tindented",
            "unicode is fine: \u{e9}\u{4e2d}\u{1f600}",
            "brackets [201~ without an escape are just text",
        ] {
            assert_eq!(control_refusal(fine), None, "{fine:?} must be accepted");
        }
    }

    /// **`\r\n` is normalised; a bare `\r` is not.**
    ///
    /// A Windows caller's paragraph break is a paragraph break — refusing it
    /// would refuse half the requests a person pastes. A lone CR is the submit
    /// key in a PTY, and guessing which one the caller meant is how one request
    /// becomes two turns.
    #[test]
    fn crlf_is_normalised_and_a_bare_cr_is_refused() {
        let composed = compose("line one\r\nline two", Some("ctx\r\nmore"), None);
        assert_eq!(composed, "line one\nline two\n\nctx\nmore");
        assert_eq!(control_refusal(&composed), None);
        assert!(control_refusal(&compose("line one\rline two", None, None)).is_some());
    }

    /// **The bytes that reach the PTY are exactly the paste and the submit.**
    ///
    /// Asserted on the DATA, not on this file's source: for a task that clears
    /// preflight, the write is start-marker + the task verbatim + end-marker,
    /// the end marker appears exactly once and at the very end, and the submit
    /// is a separate write. That conjunction is what "one paste, one turn"
    /// means, and it only holds because `control_refusal` ran first.
    #[test]
    fn the_written_bytes_are_the_paste_then_the_submit() {
        const START: &[u8] = b"\x1b[200~";
        const END: &[u8] = b"\x1b[201~";
        for id in crate::harness::contract::harness_ids() {
            let profile = crate::harness::input_profile(id).expect("a shipped profile");
            let typed = compose("do the thing", Some("src/lib/latch.ts"), None);
            assert_eq!(control_refusal(&typed), None);
            let bytes = profile.paste_bytes(&typed);
            let mut want = Vec::new();
            want.extend_from_slice(START);
            want.extend_from_slice(typed.as_bytes());
            want.extend_from_slice(END);
            assert_eq!(bytes, want, "{id}: the write is prefix + task + suffix");
            assert_eq!(
                bytes.windows(END.len()).filter(|w| *w == END).count(),
                1,
                "{id}: exactly one end marker"
            );
            assert!(bytes.ends_with(END), "{id}: and it is the last thing written");
            assert!(
                !bytes[..bytes.len() - END.len()].ends_with(b"\r"),
                "{id}: the submit is a separate write, after the settle"
            );
            assert_eq!(profile.submit, b"\r", "{id}");
        }
    }

    /// **The only bytes this module can write are one profile's paste and its
    /// submit.** No Escape, no `Ctrl-C`, no interrupt: a cancelled or timed-out
    /// delegation stops cImp waiting, and the worker finishes visibly (locked
    /// decision 6).
    ///
    /// Asserted on this file's own text, because the property is about what the
    /// code *can* do rather than about what one run did.
    #[test]
    fn the_only_bytes_written_are_the_profiles_paste_and_submit() {
        let src = include_str!("engine.rs").replace('\r', "");
        let code = crate::rustsrc::code_of("delegation/engine.rs", &src);
        let mut body = String::new();
        let mut at = 0usize;
        for (start, end) in crate::rustsrc::test_regions(&code) {
            let (start, end) = (start.min(src.len()), end.min(src.len()));
            if start > at {
                body.push_str(&src[at..start]);
            }
            at = end.max(at);
        }
        if at < src.len() {
            body.push_str(&src[at..]);
        }
        let writes: Vec<&str> = body
            .lines()
            .filter(|l| l.contains("write_through_pipeline(") && !l.trim_start().starts_with("//"))
            .collect();
        assert_eq!(
            writes.len(),
            2,
            "exactly two writes are expected (the definition-site \
             comment aside): the paste and the submit, and nothing else. A third means this \
             module grew a way to send the worker something, which locked decision 6 forbids. \
             Found {writes:#?}"
        );
        for forbidden in ["\\x1b\\x1b", "\\u{3}", "0x03", "ESC key", "interrupt("] {
            assert!(
                !body.contains(forbidden),
                "the engine must never send a cancel key, found {forbidden:?}"
            );
        }
    }

    /// **A hostile worker tab id cannot dodge the V32 screen** (locked
    /// decision 11).
    ///
    /// `wrap_external_result` screens iff the name classifies EXTERNAL, and the
    /// classifier is unknown⇒EXTERNAL — so a bare tab id named after a
    /// LOCAL-CAPABILITY tool would pass through unscreened. The
    /// `delegation__` prefix is what makes that impossible, and this pins it
    /// against every name the class table knows rather than against a sample.
    #[test]
    fn a_hostile_worker_tab_name_cannot_dodge_the_screen() {
        for evil in [
            "read_file",
            "code_search",
            "run_command",
            "offload_task",
            "graph_outline",
            "Edit",
            "WebFetch",
            "",
            "delegation",
        ] {
            let name = format!("delegation__{evil}");
            assert!(
                crate::offload::spotlight::is_external(&name),
                "`{name}` did not classify EXTERNAL — the reply would reach the driver unscreened"
            );
        }
        assert_eq!(
            screen_name(&TabId::Claude),
            format!("delegation__{}", TabId::Claude.as_str())
        );
    }

    /// **Empty is not absent** (locked decision 13) — and **a code block is an
    /// answer** (V39 review M-6).
    ///
    /// The second half is the fix: "write the regex" / "produce the JSON" /
    /// "show me the diff" are answered by one fenced block and nothing else,
    /// and every one of those was reported to the driver as `worker produced no
    /// text`. What stays non-substantive is the short scaffold list, and
    /// nothing else.
    #[test]
    fn a_non_substantive_turn_is_not_a_success() {
        for empty in [
            "",
            "   ",
            "\n\n\t",
            "```\n\n```",
            "```json\n{}\n```",
            "```json\n[]\n```",
            "```json\nnull\n```",
        ] {
            assert!(!substantive(empty), "{empty:?} must not read as an answer");
        }
        for real in [
            "Done.",
            "The file exports three symbols.",
            "Here:\n```ts\nconst a = 1;\n```\nThat is the export.",
            // The M-6 shapes: a fenced block, alone, IS the answer.
            "```ts\nconst a = 1;\n```",
            "```json\n{\"count\": 3}\n```",
            "```\n^\\d{4}-\\d{2}$\n```",
            // …and a truncated block still carries what it carries.
            "```ts\nconst a = 1;",
        ] {
            assert!(substantive(real), "{real:?} must read as an answer");
        }
    }

    /// The event name and payload shape the frontend binds to, pinned here
    /// because B2 codes against them and a rename would be silent on this side.
    #[test]
    fn the_frontend_event_contract_is_stable() {
        assert_eq!(EVENT_DELEGATION_CHANGED, "delegation-changed");
        let json = serde_json::to_value(DelegationChanged {
            in_flight: super::super::statuses(),
        })
        .expect("serializes");
        assert!(json.get("in_flight").is_some(), "{json}");
    }
}

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
    chain_from, claim, depth_from, deadline_of, is_driven, is_driving, is_taken_over, mark_submitted,
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
fn compose(task: &str, context: Option<&str>) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!("{}\n\n{}", task.trim_end(), c),
        None => task.to_string(),
    }
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
    let gate = contract::gate(CAP_DELEGATION_WORKER, &settings);
    if gate.blocked {
        let e = refuse(gate.reason.clone());
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
    let Some((worker_agent, _worker_cfg)) = ai_tab(&settings, &req.worker) else {
        return Err(deny(format!(
            "`{}` is not a configured AI tab, so it cannot be a delegation worker",
            req.worker.as_str()
        )));
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
    if !alive || flags.exited {
        return Err(deny(format!(
            "worker tab `{worker_name}` has no running process — start it and try again"
        )));
    }

    // 6. Its harness declares an input profile. `None` here is the fail-closed
    //    half of decision 16: a harness cImp cannot type into is not a worker.
    let Some(profile) = crate::harness::input_profile(worker_agent) else {
        return Err(deny(format!(
            "worker tab `{worker_name}` runs a harness with no input profile, so cImp does not \
             know how to submit a turn to it"
        )));
    };

    // 7. A substantive request that fits the profile's paste bound. Refused,
    //    never truncated — half a request is the one failure a worker cannot
    //    report, because it would answer the truncated question perfectly.
    let typed = compose(&req.task, req.context.as_deref());
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

    // 8. Acyclic, and within `delegation.max_depth` (locked decision 9). Both
    //    directions, and the refusal NAMES the chain.
    if is_driven(&driver) {
        return Err(deny(format!(
            "tab `{}` is currently being driven, so it may not drive another tab (chain: {})",
            driver.as_str(),
            chain_from(&driver).join(" -> ")
        )));
    }
    if is_driving(&req.worker) {
        return Err(deny(format!(
            "worker tab `{worker_name}` is currently driving another tab, so it may not be driven \
             (chain: {})",
            chain_from(&req.worker).join(" -> ")
        )));
    }
    let max_depth = settings.delegation.max_depth;
    let depth = depth_from(&driver).saturating_add(1);
    if depth > max_depth {
        return Err(deny(format!(
            "this delegation would nest {depth} deep and `delegation.max_depth` is {max_depth} \
             (chain: {})",
            chain_from(&driver).join(" -> ")
        )));
    }

    // 9. Idle: no output burst in progress, no prompt standing.
    if flags.output_running {
        return Err(deny(format!(
            "worker tab `{worker_name}` is mid-turn — a request typed now would land in the middle \
             of someone else's turn"
        )));
    }
    if flags.awaiting_prompt() {
        return Err(deny(format!(
            "worker tab `{worker_name}` is waiting for an answer to a prompt — answer it, then \
             delegate"
        )));
    }

    // 10. An empty input line: nothing the user has half-typed and not sent.
    //     Pasting into a composer that already holds text would send the two
    //     concatenated, under the user's name.
    let pending = {
        let map = state
            .input_lengths
            .read()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&req.worker)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    };
    if pending > 0 {
        return Err(deny(format!(
            "worker tab `{worker_name}` has {pending} characters typed and not sent — clear the \
             input line first"
        )));
    }

    // 11. A completion signal exists. Either the harness pushes CHP
    //     `assistant_text` for THIS tab, or it has a fallback reader attached.
    //     Without one the engine would type into a tab it cannot read back
    //     from, which is the silent-swallow decision 12 exists to prevent.
    let pushed = crate::harness::chp::served(
        worker_agent,
        req.worker.as_str(),
        crate::harness::chp::EV_ASSISTANT_TEXT,
    );
    let reader = crate::harness::reader::has_live_reader(&req.worker);
    if !pushed && !reader {
        return Err(deny(format!(
            "worker tab `{worker_name}` has no way to report the end of a turn (its harness pushes \
             no assistant text for this tab and no fallback reader is attached), so cImp could not \
             read the answer back — restart the tab and try again"
        )));
    }

    // ── the slot, then the lock, then the write ─────────────────────────────

    let now = crate::activity::now_ms();
    let timeout_s = req
        .timeout_s
        .filter(|s| *s > 0)
        .unwrap_or(settings.delegation.default_timeout_s)
        .max(1);
    let deadline = now.saturating_add(timeout_s.saturating_mul(1000));
    if let Err(busy) = claim(
        &req.worker,
        driver.clone(),
        driver_name.clone(),
        driver_agent.to_string(),
        req.mode,
        now,
        deadline,
    ) {
        return Err(deny(busy));
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
        &driver,
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
                    String::new(),
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
    driver: &TabId,
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
    if let Err(e) = crate::ipc::commands::write_through_pipeline(state, worker, paste).await {
        return Err(DelegationError::WorkerExited(format!(
            "could not type into worker tab `{worker_name}`: {e}"
        )));
    }
    // The settle. Not cosmetic — see the harness input profiles: a submit that
    // arrives inside the TUI's own paste debounce is evaluated against a buffer
    // it has not finished ingesting.
    tokio::time::sleep(Duration::from_millis(profile.settle_ms)).await;
    let submit = String::from_utf8_lossy(profile.submit).into_owned();
    if let Err(e) = crate::ipc::commands::write_through_pipeline(state, worker, submit).await {
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
                // Leave nothing engaged behind us; `drive` clears it too, but
                // the two must agree even if this path is reached first.
                state.read_only.set_driven(worker, None);
            }
            return Err(DelegationError::Cancelled(format!(
                "user took over worker tab `{worker_name}`"
            )));
        }

        let flags = state.tab_activity.flags(worker);
        if flags.exited {
            return Err(DelegationError::WorkerExited(format!(
                "worker tab `{worker_name}` exited while the task was running"
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
            if awaiting {
                state.read_only.set_driven(worker, None);
                relaxed = true;
            } else {
                // The signal has a consumer: the lock re-engages on the falling
                // edge. A relaxation nobody re-engages would be a delegation
                // that silently gave the keyboard back for the rest of its run.
                state.read_only.set_driven(worker, Some(driver.clone()));
                relaxed = false;
            }
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
/// Whitespace-only is the obvious half. The other half is "tool scaffolding
/// only": a final message that is nothing but a fenced block of tool chatter
/// carries no answer, and returning it as a success would report a task done
/// that produced nothing. Kept deliberately narrow — the test is *is there any
/// prose at all*, not a quality judgement.
fn substantive(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    // Strip fenced blocks; if nothing but fences and whitespace remains, the
    // message is scaffolding.
    let mut outside = String::new();
    let mut in_fence = false;
    for line in t.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            outside.push_str(line);
        }
    }
    !outside.trim().is_empty()
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

    /// **The task is typed verbatim** (locked decision 2a/10). No header, no
    /// marker, nothing a worker model could read as provenance — the
    /// attribution is client-side and in the Events row, and nowhere else.
    #[test]
    fn the_composed_request_is_the_callers_text_and_nothing_else() {
        assert_eq!(compose("summarise latch.ts", None), "summarise latch.ts");
        assert_eq!(
            compose("summarise latch.ts", Some("   ")),
            "summarise latch.ts",
            "a blank context is not a context"
        );
        let both = compose("do the thing", Some("src/lib/latch.ts"));
        assert_eq!(both, "do the thing\n\nsrc/lib/latch.ts");
        for marker in ["delegated", "cImp", "via", "[", "Context:", "Task:"] {
            assert!(
                !both.contains(marker),
                "the typed request must carry no cImp-authored text, found {marker:?}"
            );
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

    /// **Empty is not absent** (locked decision 13). Whitespace and pure tool
    /// scaffolding are not answers; anything with prose in it is.
    #[test]
    fn a_non_substantive_turn_is_not_a_success() {
        for empty in ["", "   ", "\n\n\t", "```\n\n```", "```json\n{}\n```"] {
            assert!(!substantive(empty), "{empty:?} must not read as an answer");
        }
        for real in [
            "Done.",
            "The file exports three symbols.",
            "Here:\n```ts\nconst a = 1;\n```\nThat is the export.",
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

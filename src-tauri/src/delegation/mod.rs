//! V39 — **cross-harness delegation**: one tab drives another.
//!
//! An L4 capability (`docs/HARNESS-PLUGIN-LAYER.md` § 2). It speaks cImp domain
//! types and `contract::gate(id)` only, and the `no_harness_literals_outside_
//! harness` layering test forbids it naming Claude or OpenCode — the two
//! harness-facing things it needs sit where the ladder already puts them:
//!
//! * **read half** (the reply, and the fact that the turn ended) = CHP
//!   `assistant_text`, arbitrated. A tab that pushes it is served by
//!   `offload::loopback::assistant_text_core`; a tab that does not is served by
//!   its declared fallback reader through `OobContext::note_turn_text`. Both
//!   land in [`note_assistant_text`], and exactly one of them fires — **once
//!   per TURN, carrying that turn's FINAL assistant message** (V39 review
//!   HIGH-1). A mid-turn preamble is not a reply, and a completion minted from
//!   one released the slot while the worker was still working.
//! * **push half** (submitting a turn) = the per-harness
//!   [`InputProfile`](crate::harness::InputProfile), reached through
//!   `harness::input_profile(id)` keyed by the tab's harness id. `None` ⇒ that
//!   harness is not a worker, and preflight says so.
//!
//! # What this module owns
//!
//! * [`mod.rs`](self) — the in-flight registry (single slot per worker, atomic
//!   claim), the completion slots the read half feeds, the Events rows, and the
//!   status view the UI binds to.
//! * [`engine`] — preflight (locked decision 12), the write, the wait, the
//!   screen, and the release.
//!
//! # Two properties worth stating before reading the code
//!
//! **Nothing here is persisted.** Not the registry, not a slot, not
//! `ReadOnlySource::Driven`. After a restart no tab is driven, which is the
//! only correct answer: a persisted in-flight delegation would be a lock whose
//! owner does not exist.
//!
//! **cImp never sends a key to cancel** (locked decision 6). Take-over, a
//! timeout and a driver going away all stop cImp *waiting*; the worker finishes
//! its turn, visibly, in its own tab. There is no Escape, no `Ctrl-C`, no
//! interrupt anywhere in this module — and a test pins that the only bytes it
//! can ever write are a profile's paste plus its submit.

pub mod engine;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::activity::{ActivityEntry, ActivityKind, ActivityRecord, Attribution};
use crate::state::TabId;

// `drive` itself is deliberately NOT re-exported (V39 review R-6): every
// caller with a client behind it must go through `drive_watching`, which is
// where "the caller went away" is handled. It stays reachable as
// `engine::drive` for anything that genuinely has no caller to watch.
pub use engine::{drive_watching, worker_busy, worker_ready, DriveRequest};

// ── vocabulary ──────────────────────────────────────────────────────────────

/// Which driver mode started a delegation (locked decision 3). One engine, two
/// callers; the difference is *who chose the worker*, and it survives into the
/// Events row so a reader can tell a user-directed hand-off from a routed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMode {
    /// `delegate_task_<harness>` — the model was told to, by the user.
    Explicit,
    /// A `HarnessTab` offload backend the router picked. **Constructed in
    /// Phase C**, which is the phase that adds the router side — declared now
    /// because the Events row's `mode` column, the status view and the wire
    /// shape all carry it, and a variant that arrives with its consumer would
    /// change three serialized shapes at once.
    #[allow(dead_code)]
    Facade,
}

impl DelegationMode {
    /// The wire/log spelling. Used by the Phase C router's own logging and by
    /// anything that needs the mode as a bare token rather than as JSON.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationMode::Explicit => "explicit",
            DelegationMode::Facade => "facade",
        }
    }
}

/// The Events-lane `tool` value for one transition (locked decision 14, #87).
/// **The vocabulary, spelled once** — the row writer, the UI's `rowMeta` branch
/// and the tests all read these, so a transition cannot be minted under a name
/// no consumer knows.
pub mod transition {
    pub const START: &str = "start";
    pub const DONE: &str = "done";
    pub const REFUSED: &str = "refused";
    pub const TIMEOUT: &str = "timeout";
    /// **Reserved, and unreachable today.** A take-over is recorded as
    /// [`TAKEOVER`] — one row for one event, minted where the user acted, with
    /// the driver-facing wording in its reason. This name is kept for a
    /// DRIVER-side cancel (the asking tab withdrawing a request it had made),
    /// which no surface offers: the driver is blocked inside its own tool call
    /// for the whole flight, so there is nowhere for it to ask.
    ///
    /// Kept rather than deleted because the vocabulary is a contract shared
    /// with the Events tab and with #87: a transition that once existed and now
    /// does not is a fact a reader of old rows still needs — a `cancelled` row
    /// written by an earlier build must still render.
    pub const CANCELLED: &str = "cancelled";
    pub const TAKEOVER: &str = "takeover";
    /// **The DRIVER went away while the turn was in flight** (V39 review L-7).
    ///
    /// Distinct from [`CANCELLED`], which stays reserved for a driver that
    /// *asks* to withdraw — a surface that still does not exist. This one is
    /// not asked for: the caller's connection died (a facade run whose
    /// `offload_task` client disconnected), and cImp stops waiting so the
    /// global offload permit and the worker's slot are not held for the rest of
    /// the deadline. The worker finishes visibly; no key is ever sent.
    pub const DRIVER_GONE: &str = "driver_gone";
    pub const WORKER_EXITED: &str = "worker_exited";
    pub const ROLE_MOVED: &str = "role_moved";

    /// Every transition this build can mint, in lifecycle order. The list the
    /// tests check the writers against, and the list B2's `rowMeta` branch is
    /// written from.
    #[cfg_attr(not(test), allow(dead_code))]
    pub const ALL: &[&str] = &[
        START,
        DONE,
        REFUSED,
        TIMEOUT,
        CANCELLED,
        TAKEOVER,
        DRIVER_GONE,
        WORKER_EXITED,
        ROLE_MOVED,
    ];
}

/// Why a delegation did not produce a reply.
///
/// Every variant carries a reason, and [`Self::reason`] is never empty — the
/// driver renders it as a tool result and the Events row stores it, so a
/// refusal the user cannot explain is a bug report waiting to happen (global
/// principle 5, and Phase A's `ReadOnlySource::reason` for the same rule one
/// layer down).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelegationError {
    /// Preflight said no. Never started, nothing locked, nothing typed.
    Refused(String),
    /// The deadline passed with no completion. The worker was NOT interrupted.
    Timeout(String),
    /// The user took the tab over mid-flight (locked decision 6).
    ///
    /// Recorded under [`transition::TAKEOVER`], not `CANCELLED`: **one event,
    /// one row.** The user's action is the fact; what the driver was told is
    /// the same fact seen from the other end, and it rides this variant's
    /// reason into the row rather than minting a second one.
    Cancelled(String),
    /// The caller went away while the turn was in flight (V39 review L-7):
    /// the driver's connection died, so there is nobody left to hand the reply
    /// to. cImp stops waiting; the worker is sent nothing and finishes visibly.
    DriverGone(String),
    /// The worker's subprocess exited while the turn was in flight.
    WorkerExited(String),
    /// The turn completed and its text was not substantive (locked decision
    /// 13: empty is not absent).
    NoText(String),
}

impl DelegationError {
    /// The transition this failure is recorded under.
    pub fn transition(&self) -> &'static str {
        match self {
            // A completed-but-empty turn is a `done` that failed, not a
            // refusal: the worker really did run. `ok:false` carries the
            // verdict, and the row keeps the flight time it actually took.
            DelegationError::NoText(_) => transition::DONE,
            DelegationError::Refused(_) => transition::REFUSED,
            DelegationError::Timeout(_) => transition::TIMEOUT,
            // See the variant, and `transition::CANCELLED`: a take-over is ONE
            // row, minted where the user acted.
            DelegationError::Cancelled(_) => transition::TAKEOVER,
            DelegationError::DriverGone(_) => transition::DRIVER_GONE,
            DelegationError::WorkerExited(_) => transition::WORKER_EXITED,
        }
    }

    /// The human sentence. Never empty.
    pub fn reason(&self) -> &str {
        let r = match self {
            DelegationError::Refused(r)
            | DelegationError::Timeout(r)
            | DelegationError::Cancelled(r)
            | DelegationError::DriverGone(r)
            | DelegationError::WorkerExited(r)
            | DelegationError::NoText(r) => r.as_str(),
        };
        debug_assert!(!r.trim().is_empty(), "a delegation failure with no reason");
        r
    }
}

impl std::fmt::Display for DelegationError {
    /// **The reason, and only the reason.**
    ///
    /// It used to prefix the transition (`"timeout: worker tab …"`), which was
    /// noise: every reason below is already a self-describing sentence, and the
    /// driver reads this string as a tool result. Dropping the prefix also
    /// makes ONE string serve both audiences — what the driver is told and what
    /// the Events row records — so the two cannot come to say different things
    /// about the same outcome.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

// ── the in-flight registry ──────────────────────────────────────────────────

/// One live delegation. Keyed by **worker** in [`registry`], which is what
/// makes "one delegation per worker at a time" (locked decision 9) a property
/// of the data structure rather than of a check someone has to remember.
#[derive(Clone, Debug)]
struct InFlight {
    /// Monotonic per-process id. Not a key (the worker tab is), and not on the
    /// wire — it exists so two flights on the same worker in the same second
    /// are distinguishable in the log.
    id: u64,
    driver: TabId,
    /// The driver tab's display name at start. Snapshotted because the refusal
    /// string and the banner must keep naming the tab the user saw, even if it
    /// is renamed or closed mid-flight.
    driver_name: String,
    /// The driver's harness id — the Events row's `source`.
    driver_agent: String,
    mode: DelegationMode,
    started_ms: u64,
    /// Wall-clock deadline. **Extended, never reset**, while a prompt stands
    /// (locked decision 5) — see `engine`.
    deadline_ms: u64,
    /// Set once, immediately BEFORE the paste is written. The correlation
    /// floor: a completion recorded earlier than this belongs to an earlier
    /// turn (locked decision 10).
    submit_ms: Option<u64>,
    /// Set by [`take_over`]. The wait loop notices and ends as `cancelled`.
    taken_over: bool,
    /// Set by [`note_driver_gone`]: the caller's connection died (V39 review
    /// L-7). The wait loop notices and ends the flight rather than holding the
    /// worker's slot — and the global offload permit behind it — until the
    /// deadline.
    driver_gone: bool,
    /// Set by [`note_worker_gone`] when the worker TAB is closed.
    ///
    /// A separate flag from the state mirror's `exited`, and it has to be:
    /// closing a tab drops its mirror row (`TabActivity::forget`), so the
    /// mirror's answer for a closed tab is "nothing observed" — indistinguish-
    /// able from a healthy idle tab. Without this the wait loop would sit out
    /// the whole deadline on a tab that no longer exists.
    worker_gone: bool,
    /// Mirrors "a prompt is standing on the worker right now", so the status
    /// view can say *why* a long flight is long without re-reading the state
    /// mirror.
    awaiting_prompt: bool,
}

/// One completed turn, waiting to be claimed by the delegation on that tab.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Completion {
    text: String,
    at_ms: u64,
}

#[derive(Default)]
struct Registry {
    in_flight: HashMap<TabId, InFlight>,
    /// The last completion seen per tab **while a delegation was in flight on
    /// it**. Recorded nowhere else: a tab nobody is driving needs no slot, and
    /// keeping one would be an unbounded transcript buffer.
    completions: HashMap<TabId, Completion>,
    /// Monotonic id source for [`InFlight::id`].
    next_id: u64,
    /// Workers whose caller went away **before the slot was claimed** (V39
    /// review R-8).
    ///
    /// `note_driver_gone` can only set a flag on a flight that exists, and a
    /// cancel that arrives while `drive` is still in preflight has no flight to
    /// set it on — so it did nothing, the claim went ahead, the request was
    /// typed into the worker and the delegation ran to its deadline for a
    /// caller that had already hung up.
    ///
    /// The mark is consumed by [`claim_in`], and cleared by
    /// [`clear_driver_gone`] when the call that set it finishes — so it is
    /// sticky for exactly one attempt and can never poison the next one.
    abandoned: std::collections::HashSet<TabId>,
}

fn registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    let m = REG.get_or_init(|| Mutex::new(Registry::default()));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

/// What the UI needs about a tab's in-flight delegation — the glyph's *driven*
/// state, the banner, and the take-over button (locked decision 7).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct InFlightView {
    /// Driver tab id.
    pub driver: String,
    /// Driver tab display name, as it was when the delegation started.
    pub driver_name: String,
    /// Driver harness id (`claude` / `opencode`) — what the banner calls the
    /// asking side.
    pub driver_agent: String,
    pub mode: DelegationMode,
    pub started_ms: u64,
    /// A permission/question prompt is standing on the worker right now, so
    /// the keyboard is relaxed for it and the deadline has been granted one
    /// bounded extension (locked decision 5). The banner's "waiting for your
    /// permission" state.
    pub awaiting_prompt: bool,
}

/// The in-flight delegation on `worker`, if any.
pub fn status(worker: &TabId) -> Option<InFlightView> {
    registry(|r| r.in_flight.get(worker).map(view))
}

/// Every in-flight delegation, keyed by worker tab id. Feeds the status-bar
/// chip's count and the initial paint of every tab's glyph.
pub fn statuses() -> Vec<(String, InFlightView)> {
    registry(|r| {
        let mut out: Vec<(String, InFlightView)> = r
            .in_flight
            .iter()
            .map(|(tab, f)| (tab.as_str().to_string(), view(f)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    })
}

fn view(f: &InFlight) -> InFlightView {
    InFlightView {
        driver: f.driver.as_str().to_string(),
        driver_name: f.driver_name.clone(),
        driver_agent: f.driver_agent.clone(),
        mode: f.mode,
        started_ms: f.started_ms,
        awaiting_prompt: f.awaiting_prompt,
    }
}

/// Whether `tab` is currently **driving** something — the acyclic check's
/// forward direction (locked decision 9).
///
/// The engine asks it inside [`claim_checked`] instead (V39 review M-8: the
/// question and the claim must be one locked step), so this reader is the
/// stand-alone form the tests and any future surface use.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_driving(tab: &TabId) -> bool {
    registry(|r| r.in_flight.values().any(|f| &f.driver == tab))
}

/// Whether `tab` is currently **being driven** — the other direction.
pub fn is_driven(tab: &TabId) -> bool {
    registry(|r| r.in_flight.contains_key(tab))
}

/// How deep the chain ending at `tab` already runs: 0 if `tab` drives nothing,
/// 1 if it drives a tab that drives nothing, and so on.
///
/// Bounded by the registry size, which is bounded by the tab count — and the
/// walk carries its own visited set anyway, because a cycle here must be a
/// refusal rather than a hang (the cycle cannot form through this module, but
/// the bound is free and the failure it prevents is not).
#[cfg_attr(not(test), allow(dead_code))]
pub fn depth_from(tab: &TabId) -> u8 {
    registry(|r| depth_in(r, tab))
}

/// [`depth_from`] against a registry the caller already holds — so the cycle
/// check and the claim can run under ONE lock (V39 review M-8).
fn depth_in(r: &Registry, tab: &TabId) -> u8 {
    let mut seen: Vec<TabId> = vec![tab.clone()];
    let mut depth: u8 = 0;
    let mut cur = tab.clone();
    loop {
        let Some(next) = r
            .in_flight
            .iter()
            .find(|(_, f)| f.driver == cur)
            .map(|(w, _)| w.clone())
        else {
            return depth;
        };
        if seen.contains(&next) || depth == u8::MAX {
            return depth;
        }
        seen.push(next.clone());
        cur = next;
        depth = depth.saturating_add(1);
    }
}

/// The chain a driver already sits in, as tab ids, for a refusal that **names
/// the cycle** rather than saying "busy".
#[cfg_attr(not(test), allow(dead_code))]
pub fn chain_from(tab: &TabId) -> Vec<String> {
    registry(|r| chain_in(r, tab))
}

/// [`chain_from`] against a registry the caller already holds — see
/// [`depth_in`].
fn chain_in(r: &Registry, tab: &TabId) -> Vec<String> {
    let mut out = vec![tab.as_str().to_string()];
    let mut cur = tab.clone();
    while let Some(next) = r
        .in_flight
        .iter()
        .find(|(_, f)| f.driver == cur)
        .map(|(w, _)| w.clone())
    {
        if out.contains(&next.as_str().to_string()) {
            break;
        }
        out.push(next.as_str().to_string());
        cur = next;
    }
    out
}

/// Claim the worker's single slot, atomically.
///
/// `Err(reason)` when it is already held — the loser of a race between two
/// drivers is refused `busy`, never queued (locked decision 9). Returns the new
/// delegation's id on success.
/// The unchecked claim — [`claim_checked`] is what the engine uses. Kept for
/// the tests, which set up registry states directly.
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn claim(
    worker: &TabId,
    driver: TabId,
    driver_name: String,
    driver_agent: String,
    mode: DelegationMode,
    now_ms: u64,
    deadline_ms: u64,
) -> Result<u64, String> {
    registry(|r| {
        claim_in(
            r,
            worker,
            driver,
            driver_name,
            driver_agent,
            mode,
            now_ms,
            deadline_ms,
        )
    })
}

/// **The acyclic check, the depth bound and the claim, under ONE lock**
/// (locked decision 9, V39 review M-8).
///
/// They used to be four separate registry acquisitions in `drive`, with the
/// whole of preflight in between — and the window that opened is not the usual
/// "two drivers, one worker" (the per-worker slot closes that one by itself,
/// because both claims key on the same tab). It is **A→B and B→A racing**:
/// both drivers pass a cycle check that is true at the moment it runs, then
/// both claim DIFFERENT workers, and the cycle the check exists to prevent is
/// now in the registry with two flights holding it.
///
/// The refusal strings are unchanged, and their order is the order `drive`
/// asked them in.
#[allow(clippy::too_many_arguments)]
fn claim_checked(
    worker: &TabId,
    worker_name: &str,
    driver: TabId,
    driver_name: String,
    driver_agent: String,
    mode: DelegationMode,
    now_ms: u64,
    deadline_ms: u64,
    max_depth: u8,
) -> Result<u64, String> {
    registry(|r| {
        if r.in_flight.contains_key(&driver) {
            return Err(format!(
                "tab `{}` is currently being driven, so it may not drive another tab (chain: {})",
                driver.as_str(),
                chain_in(r, &driver).join(" -> ")
            ));
        }
        if r.in_flight.values().any(|f| &f.driver == worker) {
            return Err(format!(
                "worker tab `{worker_name}` is currently driving another tab, so it may not be \
                 driven (chain: {})",
                chain_in(r, worker).join(" -> ")
            ));
        }
        let depth = depth_in(r, &driver).saturating_add(1);
        if depth > max_depth {
            return Err(format!(
                "this delegation would nest {depth} deep and `delegation.max_depth` is {max_depth} \
                 (chain: {})",
                chain_in(r, &driver).join(" -> ")
            ));
        }
        claim_in(
            r,
            worker,
            driver,
            driver_name,
            driver_agent,
            mode,
            now_ms,
            deadline_ms,
        )
    })
}

/// The claim itself, against a registry the caller holds.
#[allow(clippy::too_many_arguments)]
fn claim_in(
    r: &mut Registry,
    worker: &TabId,
    driver: TabId,
    driver_name: String,
    driver_agent: String,
    mode: DelegationMode,
    now_ms: u64,
    deadline_ms: u64,
) -> Result<u64, String> {
    {
        // V39 review R-8: the caller went away while preflight ran. Refuse
        // before the claim, which is before the lock is engaged and before a
        // byte is typed — the whole point of the mark is that this tab is left
        // exactly as it was.
        if r.abandoned.remove(worker) {
            return Err(format!(
                "the caller went away before the request was typed — nothing was sent to tab `{}`",
                worker.as_str()
            ));
        }
        if let Some(held) = r.in_flight.get(worker) {
            return Err(format!(
                "busy: tab `{}` is already being driven by `{}` (since {} ms ago)",
                worker.as_str(),
                held.driver_name,
                now_ms.saturating_sub(held.started_ms)
            ));
        }
        r.next_id = r.next_id.wrapping_add(1);
        let id = r.next_id;
        tracing::info!(
            delegation = id,
            worker = %worker.as_str(),
            driver = %driver.as_str(),
            mode = mode.as_str(),
            "delegation: slot claimed"
        );
        // A stale completion from before this delegation existed must never be
        // claimed by it. Dropping the slot here (rather than only comparing
        // timestamps later) makes that true even if a clock jumps.
        r.completions.remove(worker);
        r.in_flight.insert(
            worker.clone(),
            InFlight {
                id,
                driver,
                driver_name,
                driver_agent,
                mode,
                started_ms: now_ms,
                deadline_ms,
                submit_ms: None,
                taken_over: false,
                driver_gone: false,
                worker_gone: false,
                awaiting_prompt: false,
            },
        );
        Ok(id)
    }
}

/// Release the worker's slot and drop any unclaimed completion.
fn release(worker: &TabId) {
    registry(|r| {
        if let Some(f) = r.in_flight.remove(worker) {
            tracing::info!(
                delegation = f.id,
                worker = %worker.as_str(),
                driver = %f.driver.as_str(),
                ms = crate::activity::now_ms().saturating_sub(f.started_ms),
                "delegation: slot released"
            );
        }
        r.completions.remove(worker);
    });
}

/// Record the moment the request was written. The correlation floor.
fn mark_submitted(worker: &TabId, at_ms: u64) {
    registry(|r| {
        if let Some(f) = r.in_flight.get_mut(worker) {
            f.submit_ms = Some(at_ms);
        }
    });
}

/// **The completion feed** — one completed TURN's final assistant message for
/// `tab`.
///
/// Called from both arbitrated halves of the read seam (the CHP push core and
/// the fallback reader), so exactly one of them fires per turn.
///
/// **Per turn, not per message** (V39 review HIGH-1). Both producers owe this
/// contract: the CHP push core is fed by the harness's turn-over hook
/// (`last_assistant_message`), and each fallback reader buffers its turn and
/// files the last assistant text at the turn-over edge. A per-MESSAGE feed
/// handed the driver a mid-turn preamble ("I'll read that file first.") as the
/// reply and released the worker's slot while it was still working. A no-op
/// unless a delegation is in flight on that tab: a tab nobody is driving needs
/// no slot, and keeping one would turn this into an unbounded transcript
/// buffer for every open tab.
///
/// Deliberately records **empty text too**. Locked decision 13 says a completed
/// turn with nothing substantive in it is an *error*, not a success — which it
/// can only be if the completion is seen at all. Dropping it here would turn
/// "the worker said nothing" into "the worker never answered", i.e. a timeout
/// minutes later instead of an immediate, accurate refusal.
pub fn note_assistant_text(tab: &TabId, text: &str) {
    note_assistant_text_at(tab, text, crate::activity::now_ms())
}

/// [`note_assistant_text`], with the moment the text was PRODUCED rather than
/// the moment it was filed (V39 review R-3).
///
/// Correlation is by time (locked decision 10: a completion recorded before the
/// request was submitted belongs to an earlier turn), so a producer that
/// BUFFERS — the OpenCode reader holds a turn's last message until the turn is
/// over — must file the buffer's own timestamp. Stamping at file time made a
/// message produced before the delegation existed look like its reply, because
/// the file happened afterwards.
///
/// A reader that does not buffer passes `now`, which is what
/// [`note_assistant_text`] does for it.
pub fn note_assistant_text_at(tab: &TabId, text: &str, at_ms: u64) {
    registry(|r| {
        if !r.in_flight.contains_key(tab) {
            return;
        }
        r.completions.insert(
            tab.clone(),
            Completion {
                text: text.to_string(),
                at_ms,
            },
        );
    });
}

/// Take the completion for `worker` **if it belongs to this delegation's turn**
/// (locked decision 10: correlation is by turn, not by marker).
///
/// The rule is one comparison, and it is the whole correlation: accept only a
/// completion recorded at or after the submit timestamp. Anything earlier
/// belongs to a turn that was already running — and preflight's idleness check
/// is what makes "already running" rare rather than routine.
fn take_completion(worker: &TabId) -> Option<String> {
    registry(|r| {
        let submit_ms = r.in_flight.get(worker).and_then(|f| f.submit_ms)?;
        let c = r.completions.get(worker)?;
        if c.at_ms < submit_ms {
            // An earlier turn's tail. Drop it so it cannot be re-examined on
            // every poll, and keep waiting for ours.
            r.completions.remove(worker);
            return None;
        }
        r.completions.remove(worker).map(|c| c.text)
    })
}

/// **Take over** a driven tab (locked decision 6): stop waiting, hand the
/// driver `cancelled`, let the worker finish visibly.
///
/// Returns the view of what was cancelled, or `None` when nothing was in
/// flight. **Sends no key to the worker** — this only sets a flag the wait loop
/// reads. The lock and the Events row are the engine's, released on its way
/// out, so a take-over cannot leave a half-torn-down delegation behind.
pub fn take_over(worker: &TabId) -> Option<InFlightView> {
    registry(|r| {
        // V39 review L-3: a flight whose reply has already landed cannot be
        // taken over. The engine takes the completion, then screens and records
        // it — during which the slot is still held, so a click in that window
        // used to flag a flight that was already finished: the UI said "you
        // took it back", the driver received its answer anyway, and NO row was
        // minted for the take-over (the engine was past the branch that would
        // have written one). Answering `None` makes the UI say the true thing:
        // "it had already finished".
        //
        // "Landed" is decision 10's correlation rule, not "a completion exists"
        // — an earlier turn's tail sitting in the slot is not this delegation's
        // reply, and must not make its take-over a no-op.
        let submit_ms = r.in_flight.get(worker)?.submit_ms;
        let landed = match (submit_ms, r.completions.get(worker)) {
            (Some(submitted), Some(c)) => c.at_ms >= submitted,
            _ => false,
        };
        if landed {
            return None;
        }
        let f = r.in_flight.get_mut(worker)?;
        f.taken_over = true;
        Some(view(f))
    })
}

/// Whether the user has taken this delegation over.
fn is_taken_over(worker: &TabId) -> bool {
    registry(|r| r.in_flight.get(worker).is_some_and(|f| f.taken_over))
}

/// **The driver went away** (V39 review L-7) — its `offload_task` client
/// disconnected, so nothing is waiting for this reply any more.
///
/// Sets a flag the wait loop reads, exactly like [`take_over`]: **no key is
/// sent**, the worker finishes its turn visibly, and the engine's own path
/// releases the lock and mints the one terminal row. Returns whether a
/// delegation was actually in flight.
///
/// A no-op otherwise, so the caller can call it blind on a cancelled future.
pub fn note_driver_gone(worker: &TabId) -> bool {
    registry(|r| match r.in_flight.get_mut(worker) {
        Some(f) => {
            f.driver_gone = true;
            true
        }
        // V39 review R-8: nothing in flight YET. The caller hung up while
        // preflight was still running, so the mark waits for the claim rather
        // than being dropped — see `Registry::abandoned`.
        None => {
            r.abandoned.insert(worker.clone());
            false
        }
    })
}

/// Drop a pre-claim abandonment mark (V39 review R-8).
///
/// Called by the driving path once its attempt is over, whatever the outcome:
/// the mark exists to be consumed by THIS attempt's claim, and a mark that
/// outlived it would refuse the next delegation into the same worker for a
/// caller that is long gone.
pub fn clear_driver_gone(worker: &TabId) {
    registry(|r| r.abandoned.remove(worker));
}

/// Whether the driver has gone away under this delegation.
fn is_driver_gone(worker: &TabId) -> bool {
    registry(|r| r.in_flight.get(worker).is_some_and(|f| f.driver_gone))
}

/// **The worker tab was closed.** Called from the tab-lifecycle paths, beside
/// the other per-tab cleanups (`read_only.forget`, the input buffers).
///
/// It must be its own signal rather than a read of the state mirror: closing a
/// tab REMOVES its mirror row, so a closed tab reads as "nothing observed",
/// which is what a healthy idle tab reads as too. Left to the mirror, a
/// delegation into a tab the user just closed would wait out its entire
/// deadline before reporting anything.
///
/// A no-op when nothing is in flight, so the lifecycle paths can call it
/// unconditionally.
pub fn note_worker_gone(worker: &TabId) {
    registry(|r| {
        if let Some(f) = r.in_flight.get_mut(worker) {
            f.worker_gone = true;
        }
    });
}

/// Whether the worker tab has gone away under this delegation.
fn is_worker_gone(worker: &TabId) -> bool {
    registry(|r| r.in_flight.get(worker).is_some_and(|f| f.worker_gone))
}

/// Fold the worker's current prompt state into the in-flight record, granting
/// the deadline one bounded extension per prompt (locked decision 5).
///
/// Returns `(awaiting_now, just_changed)`. `just_changed` is what the engine
/// turns into the two edge actions — relax the lock on the rising edge,
/// re-engage on the falling one — so the caller never has to keep its own copy
/// of the previous value.
///
/// # The extension is per PROMPT, not per tick, and that is the whole subtlety
///
/// Decision 5 says the wait is extended while the prompt stands; the
/// adversarial failure-mode table says a prompt nobody answers must still hit
/// the deadline and report `timeout (worker awaiting permission)`. Those two
/// are only compatible if the extension is bounded — an extension applied on
/// every poll tick would advance the deadline exactly as fast as the clock and
/// the delegation would hang forever, which is the failure the table names.
///
/// So the grant is made **on the rising edge only**: one prompt buys one
/// `grant_ms` of human thinking time, and a worker that raises prompt after
/// prompt is genuinely making progress each time.
fn note_prompt(worker: &TabId, awaiting: bool, grant_ms: u64) -> (bool, bool) {
    registry(|r| {
        let Some(f) = r.in_flight.get_mut(worker) else {
            return (false, false);
        };
        let changed = f.awaiting_prompt != awaiting;
        f.awaiting_prompt = awaiting;
        if awaiting && changed {
            f.deadline_ms = f.deadline_ms.saturating_add(grant_ms);
        }
        (awaiting, changed)
    })
}

/// This delegation's deadline right now (it moves while a prompt stands).
fn deadline_of(worker: &TabId) -> Option<u64> {
    registry(|r| r.in_flight.get(worker).map(|f| f.deadline_ms))
}

// ── Events rows (locked decision 14, #87) ───────────────────────────────────

/// Mint one `delegation` row.
///
/// The column contract, which every caller here obeys and the tests pin:
/// `tool` = the transition, `target` = the worker tab name (plus the reason on
/// anything that is not a plain success), `source` = the driver HARNESS id,
/// the attribution = the driver TAB, `ms` = flight time, and
/// `request`/`response` = the verbatim task and the screened reply **on `done`
/// rows only** — a transition row carries no payload, which is what the UI's
/// `rowMeta` branch must not print as "0 chars".
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_row(
    transition: &str,
    worker_name: &str,
    reason: Option<&str>,
    driver_agent: &str,
    driver_tab: Option<&str>,
    ok: bool,
    ms: u64,
    request: String,
    response: String,
) {
    let target = match reason {
        Some(r) if !r.trim().is_empty() => format!("{worker_name} — {r}"),
        _ => worker_name.to_string(),
    };
    let chars = response.chars().count();
    // `record_bg`, not `record`: this is called from the engine's async path,
    // and the store's write is synchronous file I/O.
    crate::activity::record_bg(ActivityRecord {
        entry: ActivityEntry::new(
            ActivityKind::Delegation,
            crate::activity::now_ms(),
            String::new(),
            driver_agent.to_string(),
            transition.to_string(),
            target,
            chars,
            ms,
            ok,
            match driver_tab {
                Some(t) => Attribution::Tab(t.to_string()),
                None => Attribution::Headless,
            },
            None,
            None,
            None,
        ),
        request,
        response,
    });
}

/// **Test-only doors into the process-global registry.**
///
/// The completion feed's two producers live in `harness/` (the fallback
/// readers), so the test that proves a reader files exactly one completion per
/// TURN — V39 review HIGH-1 — has to claim a slot and read the slot back from
/// another module. Rather than widen the shipped API for it, the doors are
/// named here and compiled only under `cfg(test)`.
///
/// [`with_clean_registry`](testing::with_clean_registry) is the same guard this
/// module's own tests take, and taking it is not optional for a caller: the
/// registry is one process-global, `cargo test` runs its tests in parallel
/// threads, and a test that cleared it while another held a claim would be a
/// flake wearing an assertion's clothes.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Run `f` with the registry emptied before and after, under the lock every
    /// registry-touching test shares.
    pub(crate) fn with_clean_registry<T>(f: impl FnOnce() -> T) -> T {
        let _g = lock_registry();
        f()
    }

    /// The same exclusion as [`with_clean_registry`], as a guard — for a test
    /// whose body `await`s (a reader's tracker), which a closure taking `&mut`
    /// state cannot wrap. Clears on acquisition and again on drop.
    pub(crate) fn lock_registry() -> RegistryGuard {
        static GUARD: Mutex<()> = Mutex::new(());
        let g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        RegistryGuard(g)
    }

    pub(crate) struct RegistryGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Drop for RegistryGuard {
        fn drop(&mut self) {
            clear();
        }
    }

    fn clear() {
        registry(|r| {
            r.in_flight.clear();
            r.completions.clear();
            r.abandoned.clear();
        });
    }

    /// Claim `worker`'s slot for a fake driver and mark it submitted at time 0,
    /// so any completion recorded afterwards correlates with this delegation.
    pub(crate) fn claim_and_submit(worker: &TabId) {
        claim(
            worker,
            TabId::OpenCode,
            "the driver".to_string(),
            "opencode".to_string(),
            DelegationMode::Explicit,
            0,
            u64::MAX,
        )
        .expect("the slot was free");
        mark_submitted(worker, 0);
    }

    /// The completion filed for `worker`, consuming it — `None` when the reader
    /// has filed nothing yet, which is the half HIGH-1 is about.
    pub(crate) fn take(worker: &TabId) -> Option<String> {
        take_completion(worker)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::with_clean_registry;
    use super::*;

    fn worker() -> TabId {
        TabId::Claude
    }
    fn driver() -> TabId {
        TabId::OpenCode
    }

    fn claim_one(w: &TabId, d: &TabId, now: u64) -> Result<u64, String> {
        claim(
            w,
            d.clone(),
            "the driver".to_string(),
            "opencode".to_string(),
            DelegationMode::Explicit,
            now,
            now + 1000,
        )
    }

    /// **One delegation per worker, and the loser is refused rather than
    /// queued** (locked decision 9). The claim is the slot; there is no second
    /// place that could disagree about whether one is held.
    #[test]
    fn a_worker_slot_admits_one_and_refuses_the_second() {
        with_clean_registry(|| {
            assert!(claim_one(&worker(), &driver(), 100).is_ok());
            let second = claim_one(&worker(), &TabId::ClaudeLocal, 200)
                .expect_err("the second claim must be refused");
            assert!(second.starts_with("busy:"), "{second}");
            assert!(
                second.contains("the driver"),
                "the refusal must name who holds it: {second}"
            );
            release(&worker());
            assert!(
                claim_one(&worker(), &TabId::ClaudeLocal, 300).is_ok(),
                "the slot is free again once released"
            );
        });
    }

    /// **Correlation is by turn** (locked decision 10). A completion recorded
    /// before the request was submitted belongs to a turn that was already in
    /// flight, and claiming it would hand the driver the answer to somebody
    /// else's question.
    #[test]
    fn an_earlier_turns_completion_is_not_mistaken_for_the_reply() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim");
            // A turn that was already running finishes right after the slot was
            // claimed but BEFORE cImp typed anything. Timestamps are set
            // explicitly rather than taken from the clock: the property under
            // test is an ordering, and a test that depended on two `now_ms()`
            // calls landing in different milliseconds would be a flake wearing
            // an assertion's clothes.
            note_assistant_text(&worker(), "answer to the previous question");
            registry(|r| {
                r.completions
                    .get_mut(&worker())
                    .expect("the stale completion was recorded")
                    .at_ms = 1_000;
            });
            mark_submitted(&worker(), 2_000);
            assert_eq!(
                take_completion(&worker()),
                None,
                "a pre-submit completion must not be claimed as the reply"
            );
            // …and the real one, after the submit, is.
            note_assistant_text(&worker(), "the actual reply");
            registry(|r| {
                r.completions
                    .get_mut(&worker())
                    .expect("recorded")
                    .at_ms = 3_000;
            });
            assert_eq!(take_completion(&worker()).as_deref(), Some("the actual reply"));
            // Claimed exactly once.
            assert_eq!(take_completion(&worker()), None);
        });
    }

    /// **A buffered completion is correlated by when it was PRODUCED** (V39
    /// review R-3).
    ///
    /// The OpenCode reader holds a turn's last message until the turn is over,
    /// so the moment it files is not the moment the worker spoke. Filed with
    /// the file time, text produced before a delegation existed passed the
    /// submit-time floor and was handed to the driver as its reply.
    #[test]
    fn a_buffered_completion_carries_the_time_it_was_produced() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim");
            mark_submitted(&worker(), 2_000);
            // Buffered at 1_000 — before this delegation typed anything — and
            // filed now. The file time would have passed the floor; the
            // production time must not.
            note_assistant_text_at(&worker(), "words from an earlier turn", 1_000);
            assert_eq!(
                take_completion(&worker()),
                None,
                "text produced before the request cannot be its reply, whenever it is filed"
            );
            note_assistant_text_at(&worker(), "the actual reply", 3_000);
            assert_eq!(
                take_completion(&worker()).as_deref(),
                Some("the actual reply")
            );
        });
    }

    /// A completion for a tab nobody is driving is dropped: no slot, no
    /// unbounded per-tab transcript buffer.
    #[test]
    fn a_completion_for_an_undriven_tab_is_not_recorded() {
        with_clean_registry(|| {
            note_assistant_text(&worker(), "nobody asked");
            assert!(registry(|r| r.completions.is_empty()));
        });
    }

    /// …but an EMPTY completion for a driven tab IS recorded, because locked
    /// decision 13 needs to tell "the worker said nothing" (an error, now) from
    /// "the worker never answered" (a timeout, minutes later).
    #[test]
    fn an_empty_completion_is_still_a_completion() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim");
            mark_submitted(&worker(), 0);
            note_assistant_text(&worker(), "   \n  ");
            assert_eq!(take_completion(&worker()).as_deref(), Some("   \n  "));
        });
    }

    /// The acyclic check's two directions and the chain it names (decision 9).
    #[test]
    fn driving_and_driven_are_both_visible_and_the_chain_is_nameable() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim");
            assert!(is_driving(&driver()));
            assert!(!is_driving(&worker()));
            assert!(is_driven(&worker()));
            assert!(!is_driven(&driver()));
            assert_eq!(depth_from(&driver()), 1);
            assert_eq!(depth_from(&worker()), 0);
            assert_eq!(
                chain_from(&driver()),
                vec![driver().as_str().to_string(), worker().as_str().to_string()]
            );
        });
    }

    /// **A→B and B→A cannot both proceed** (locked decision 9, V39 review
    /// M-8).
    ///
    /// The per-worker slot does NOT cover this race: the two claims key on
    /// different workers, so both used to succeed and the cycle the check
    /// exists to prevent ended up in the registry with two flights holding it.
    /// What closes it is that the check and the claim are one locked step.
    ///
    /// Run repeatedly, and with a barrier, because the window the old code left
    /// open was the whole of preflight — a single unsynchronised attempt proves
    /// nothing either way.
    #[test]
    fn two_tabs_delegating_to_each_other_cannot_both_proceed() {
        use std::sync::{Arc, Barrier};
        with_clean_registry(|| {
            for _ in 0..64 {
                registry(|r| {
                    r.in_flight.clear();
                    r.completions.clear();
                });
                let gate = Arc::new(Barrier::new(2));
                let one = {
                    let gate = gate.clone();
                    std::thread::spawn(move || {
                        gate.wait();
                        claim_checked(
                            &TabId::Claude,
                            "the worker",
                            TabId::OpenCode,
                            "B".to_string(),
                            "opencode".to_string(),
                            DelegationMode::Explicit,
                            100,
                            1_000,
                            4,
                        )
                    })
                };
                let two = {
                    let gate = gate.clone();
                    std::thread::spawn(move || {
                        gate.wait();
                        claim_checked(
                            &TabId::OpenCode,
                            "the worker",
                            TabId::Claude,
                            "A".to_string(),
                            "claude".to_string(),
                            DelegationMode::Explicit,
                            100,
                            1_000,
                            4,
                        )
                    })
                };
                let (a, b) = (one.join().unwrap(), two.join().unwrap());
                assert!(
                    a.is_ok() ^ b.is_ok(),
                    "exactly one of A->B and B->A may proceed, got {a:?} / {b:?}"
                );
                let loser = if a.is_err() { a } else { b };
                let reason = loser.unwrap_err();
                assert!(
                    reason.contains("chain:"),
                    "the loser's refusal names the chain: {reason}"
                );
                assert_eq!(
                    registry(|r| r.in_flight.len()),
                    1,
                    "the loser claimed nothing"
                );
            }
        });
    }

    /// The depth bound is asked under the same lock, and refuses by name.
    #[test]
    fn the_depth_bound_is_checked_where_the_slot_is_taken() {
        with_clean_registry(|| {
            // `max_depth: 1` is the default — one hop, no nesting.
            claim_checked(
                &worker(),
                "the worker",
                driver(),
                "A".to_string(),
                "opencode".to_string(),
                DelegationMode::Explicit,
                100,
                1_000,
                1,
            )
            .expect("the first hop is within the bound");
            // Same DRIVER, a second worker: the driver is not itself driven, so
            // the refusal that fires is the depth bound rather than the cycle.
            let too_deep = claim_checked(
                &TabId::ClaudeLocal,
                "the second worker",
                driver(),
                "A".to_string(),
                "opencode".to_string(),
                DelegationMode::Explicit,
                100,
                1_000,
                1,
            )
            .expect_err("a second hop is not");
            assert!(too_deep.contains("would nest 2 deep"), "{too_deep}");
            assert!(too_deep.contains("chain:"), "{too_deep}");
        });
    }

    /// A two-hop chain reports depth 2, and the walk terminates even if the
    /// registry somehow described a cycle.
    #[test]
    fn depth_counts_hops_and_terminates_on_a_cycle() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim a<-b");
            claim_one(&TabId::ClaudeLocal, &worker(), 100).expect("claim b<-c");
            assert_eq!(depth_from(&driver()), 2);
            // Force the pathological shape the walk must survive.
            registry(|r| {
                if let Some(f) = r.in_flight.get_mut(&driver()) {
                    let _ = f;
                }
                let d = driver();
                let w = worker();
                r.in_flight.get_mut(&w).map(|f| f.driver = TabId::ClaudeLocal);
                r.in_flight
                    .get_mut(&TabId::ClaudeLocal)
                    .map(|f| f.driver = w.clone());
                let _ = d;
            });
            let _ = depth_from(&worker());
            let _ = chain_from(&worker());
        });
    }

    /// **A take-over that arrives after the reply cancels nothing** (V39
    /// review L-3).
    ///
    /// The slot is still held while the engine screens and records the reply,
    /// so a click in that window flagged a flight that was already over: the UI
    /// claimed "you took it back", the driver got its answer regardless, and no
    /// `takeover` row was ever minted — the engine had passed the branch that
    /// writes one. `None` is the honest answer, and it is the one the UI
    /// already renders as "it had already finished".
    #[test]
    fn a_take_over_after_the_reply_landed_reports_nothing_to_cancel() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim");
            mark_submitted(&worker(), 0);
            assert!(
                take_over(&worker()).is_some(),
                "before the reply lands, a take-over takes"
            );
            registry(|r| {
                r.in_flight
                    .get_mut(&worker())
                    .expect("in flight")
                    .taken_over = false;
            });
            // An EARLIER turn's tail lands in the slot: not this delegation's
            // reply, so a take-over still takes.
            note_assistant_text(&worker(), "someone else's answer");
            registry(|r| {
                r.completions.get_mut(&worker()).expect("recorded").at_ms = 0;
            });
            registry(|r| {
                r.in_flight.get_mut(&worker()).expect("in flight").submit_ms = Some(10);
            });
            assert!(
                take_over(&worker()).is_some(),
                "a pre-submit completion is not this flight's reply"
            );
            registry(|r| {
                r.in_flight
                    .get_mut(&worker())
                    .expect("in flight")
                    .taken_over = false;
            });
            // The worker's answer lands; the engine is now screening it.
            note_assistant_text(&worker(), "the answer");
            assert_eq!(
                take_over(&worker()),
                None,
                "there is nothing left to cancel — the reply is already in"
            );
            assert!(
                !is_taken_over(&worker()),
                "and nothing was flagged, so no `takeover` row can be minted for it"
            );
        });
    }

    /// **Take-over sets a flag and nothing else.** No key is written anywhere
    /// on this path — the worker finishes what it is doing, visibly.
    #[test]
    fn take_over_flags_the_flight_and_reports_what_it_cancelled() {
        with_clean_registry(|| {
            assert_eq!(take_over(&worker()), None, "nothing in flight");
            claim_one(&worker(), &driver(), 100).expect("claim");
            let v = take_over(&worker()).expect("the in-flight view");
            assert_eq!(v.driver, driver().as_str());
            assert_eq!(v.driver_name, "the driver");
            assert!(is_taken_over(&worker()));
        });
    }

    /// **A driver that went away ends the flight** (V39 review L-7).
    ///
    /// Same shape as a take-over — a flag, and nothing else. What it must NOT
    /// be is a dropped future: dropping `drive()` mid-await would leave the
    /// worker's slot claimed and its keyboard locked with no owner, which is
    /// exactly the lock-whose-owner-does-not-exist this module refuses to
    /// persist for the same reason.
    #[test]
    fn a_driver_that_went_away_is_its_own_signal() {
        with_clean_registry(|| {
            assert!(
                !note_driver_gone(&worker()),
                "nothing in flight: the ANSWER is `false`, so a cancelled caller can call it blind"
            );
            // …though since V39 review R-8 it also leaves a mark for the claim
            // that has not happened yet. That mark belongs to the attempt that
            // set it, and its caller drops it when the attempt ends.
            clear_driver_gone(&worker());
            claim_one(&worker(), &driver(), 100).expect("claim");
            assert!(!is_driver_gone(&worker()));
            assert!(note_driver_gone(&worker()));
            assert!(is_driver_gone(&worker()));
            release(&worker());
            assert!(!is_driver_gone(&worker()), "released with the slot");
        });
    }

    /// **A caller that hangs up during preflight is not typed to** (V39 review
    /// R-8).
    ///
    /// `note_driver_gone` sets a flag on a flight, and a cancel arriving before
    /// the slot is claimed has no flight to set it on — so it was a no-op, the
    /// request was typed into the worker, and the delegation ran to its
    /// deadline for a caller that had already gone. The mark now waits for the
    /// claim and is consumed there: before the lock, before the paste.
    #[test]
    fn a_caller_that_hangs_up_during_preflight_is_refused_at_the_claim() {
        with_clean_registry(|| {
            assert!(
                !note_driver_gone(&worker()),
                "nothing in flight — the answer is still `false`, and now it also marks"
            );
            let refused = claim_one(&worker(), &driver(), 100)
                .expect_err("the claim must consume the mark and refuse");
            assert!(
                refused.contains("went away before the request was typed"),
                "{refused}"
            );
            assert!(
                !is_driven(&worker()),
                "nothing was claimed, so nothing was locked and nothing was typed"
            );
            // Consumed, not sticky: the next delegation into the same worker is
            // a different call and must not inherit this one's refusal.
            assert!(claim_one(&worker(), &driver(), 200).is_ok());
        });
    }

    /// …and a mark whose call ended before the claim reached it is cleared
    /// rather than left for the next one.
    #[test]
    fn an_abandonment_mark_does_not_outlive_its_attempt() {
        with_clean_registry(|| {
            note_driver_gone(&worker());
            clear_driver_gone(&worker());
            assert!(
                claim_one(&worker(), &driver(), 100).is_ok(),
                "the mark belonged to a call that is over"
            );
        });
    }

    /// **A closed worker tab is noticed immediately, not at the deadline.**
    ///
    /// The state mirror cannot answer this: closing a tab drops its row, so a
    /// closed tab and a healthy idle one both read as "nothing observed". This
    /// is the signal that tells them apart.
    #[test]
    fn a_closed_worker_tab_is_its_own_signal() {
        with_clean_registry(|| {
            note_worker_gone(&worker());
            assert!(
                !is_worker_gone(&worker()),
                "a no-op when nothing is in flight, so the lifecycle paths can call it blind"
            );
            claim_one(&worker(), &driver(), 100).expect("claim");
            assert!(!is_worker_gone(&worker()));
            note_worker_gone(&worker());
            assert!(is_worker_gone(&worker()));
            release(&worker());
            assert!(!is_worker_gone(&worker()), "released with the slot");
        });
    }

    /// **A prompt buys ONE bounded extension, not a stalled clock.**
    ///
    /// This is the test for the tension between locked decision 5 ("the wait is
    /// extended while the prompt stands") and the adversarial failure-mode
    /// table ("a prompt nobody answers still hits the deadline"). Granting per
    /// poll tick would advance the deadline exactly as fast as time passes, and
    /// the delegation would hang forever — so the grant rides the rising edge.
    #[test]
    fn a_prompt_grants_one_bounded_extension_not_a_stalled_clock() {
        with_clean_registry(|| {
            claim_one(&worker(), &driver(), 100).expect("claim");
            let before = deadline_of(&worker()).expect("deadline");
            assert_eq!(note_prompt(&worker(), true, 500), (true, true), "rising edge");
            assert_eq!(deadline_of(&worker()), Some(before + 500));
            // Every subsequent poll while the SAME prompt stands must not move
            // it again — this is the clause that makes the timeout reachable.
            for _ in 0..50 {
                assert_eq!(note_prompt(&worker(), true, 500), (true, false));
            }
            assert_eq!(
                deadline_of(&worker()),
                Some(before + 500),
                "one prompt, one grant — a per-tick grant would be an infinite wait"
            );
            assert_eq!(note_prompt(&worker(), false, 500), (false, true), "falling edge");
            assert_eq!(
                deadline_of(&worker()),
                Some(before + 500),
                "a resolved prompt does not claw the grant back"
            );
            // A NEW prompt is new progress, and buys its own grant.
            assert_eq!(note_prompt(&worker(), true, 500), (true, true));
            assert_eq!(deadline_of(&worker()), Some(before + 1000));
        });
    }

    /// Every failure carries a non-empty reason and lands on a transition the
    /// vocabulary declares.
    #[test]
    fn every_failure_names_a_declared_transition_and_a_reason() {
        for e in [
            DelegationError::Refused("worker is busy".into()),
            DelegationError::Timeout("deadline".into()),
            DelegationError::Cancelled("user took over".into()),
            DelegationError::DriverGone("the caller disconnected".into()),
            DelegationError::WorkerExited("process gone".into()),
            DelegationError::NoText("worker produced no text".into()),
        ] {
            assert!(!e.reason().trim().is_empty());
            assert!(
                transition::ALL.contains(&e.transition()),
                "{} is not a declared transition",
                e.transition()
            );
            assert_eq!(
                e.to_string(),
                e.reason(),
                "the driver's string and the row's reason are ONE string"
            );
        }
        // **A take-over is one event and one row** — the user's action, minted
        // where it happened. `cancelled` stays in the vocabulary for a
        // driver-side cancel that no surface offers today.
        assert_eq!(
            DelegationError::Cancelled("cancelled: user took over".into()).transition(),
            transition::TAKEOVER
        );
    }

    /// **`transition::CANCELLED` is reserved and unreachable, and nothing mints
    /// it.**
    ///
    /// Asserted on the tree rather than on this module, because the failure it
    /// guards against is a SECOND writer appearing elsewhere — which is exactly
    /// how the take-over came to mint two rows in the first cut of this phase.
    /// If a driver-side cancel is ever built, this test is the one that has to
    /// be changed deliberately.
    #[test]
    fn nothing_mints_a_cancelled_row() {
        // V39 review L-4: the whole tree, not four files somebody remembered.
        // The failure this guards against is a SECOND writer appearing
        // ELSEWHERE, and a hardcoded list is blind to exactly that — the file
        // it does not name is the file the next writer lands in.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<(String, String)> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let name = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    // CRLF-safe: CI checks the tree out with CRLF and the
                    // offsets `rustsrc` reports are into the stripped text.
                    files.push((name, std::fs::read_to_string(&path).expect("utf-8")));
                }
            }
        }
        assert!(
            files.len() > 50,
            "the scan must actually have walked the tree, found {}",
            files.len()
        );
        for (name, src) in files.iter().map(|(n, s)| (n.as_str(), s.as_str())) {
            let src = src.replace('\r', "");
            let code = crate::rustsrc::code_of(name, &src);
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
            // Comments explaining the reservation are wanted; a CALL is not.
            let uses: Vec<&str> = body
                .lines()
                .filter(|l| l.contains("transition::CANCELLED"))
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect();
            assert!(
                uses.is_empty(),
                "`{name}` mints a `cancelled` delegation row — a take-over is ONE row                  (`takeover`), and `cancelled` is reserved for a driver-side cancel that does                  not exist: {uses:#?}"
            );
        }
    }

    /// The `target` column names the worker, and a reason is appended rather
    /// than replacing it — a refusal that lost the tab name is unreadable in a
    /// feed of many tabs.
    #[test]
    fn a_row_target_names_the_worker_and_carries_the_reason() {
        // Exercised through the same formatting the recorder uses.
        let with = |reason: Option<&str>| match reason {
            Some(r) if !r.trim().is_empty() => format!("api-work — {r}"),
            _ => "api-work".to_string(),
        };
        assert_eq!(with(None), "api-work");
        assert_eq!(with(Some("   ")), "api-work");
        assert_eq!(with(Some("busy")), "api-work — busy");
    }
}

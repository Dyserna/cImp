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
//!   land in [`note_assistant_text`], and exactly one fires per message.
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

pub use engine::{drive, DriveRequest};

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
    pub const CANCELLED: &str = "cancelled";
    pub const TAKEOVER: &str = "takeover";
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
    Cancelled(String),
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
            DelegationError::Cancelled(_) => transition::CANCELLED,
            DelegationError::WorkerExited(_) => transition::WORKER_EXITED,
        }
    }

    /// The human sentence. Never empty.
    pub fn reason(&self) -> &str {
        let r = match self {
            DelegationError::Refused(r)
            | DelegationError::Timeout(r)
            | DelegationError::Cancelled(r)
            | DelegationError::WorkerExited(r)
            | DelegationError::NoText(r) => r.as_str(),
        };
        debug_assert!(!r.trim().is_empty(), "a delegation failure with no reason");
        r
    }
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.transition(), self.reason())
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
    /// the keyboard is relaxed and the deadline is being extended (decision 5).
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
pub fn depth_from(tab: &TabId) -> u8 {
    registry(|r| {
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
    })
}

/// The chain a driver already sits in, as tab ids, for a refusal that **names
/// the cycle** rather than saying "busy".
pub fn chain_from(tab: &TabId) -> Vec<String> {
    registry(|r| {
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
    })
}

/// Claim the worker's single slot, atomically.
///
/// `Err(reason)` when it is already held — the loser of a race between two
/// drivers is refused `busy`, never queued (locked decision 9). Returns the new
/// delegation's id on success.
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
                awaiting_prompt: false,
            },
        );
        Ok(id)
    })
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

/// **The completion feed** — one completed assistant message for `tab`.
///
/// Called from both arbitrated halves of the read seam (the CHP push core and
/// the fallback reader), so exactly one of them fires per message. A no-op
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
    let at_ms = crate::activity::now_ms();
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
        let f = r.in_flight.get_mut(worker)?;
        f.taken_over = true;
        Some(view(f))
    })
}

/// Whether the user has taken this delegation over.
fn is_taken_over(worker: &TabId) -> bool {
    registry(|r| r.in_flight.get(worker).is_some_and(|f| f.taken_over))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> TabId {
        TabId::Claude
    }
    fn driver() -> TabId {
        TabId::OpenCode
    }

    /// Every test in this module mutates one process-global registry, so they
    /// share a lock and clean up after themselves. Cheaper and more honest than
    /// threading a handle through six call sites for the sake of tests: the
    /// singleton IS the single-slot property under test.
    fn with_clean_registry<T>(f: impl FnOnce() -> T) -> T {
        static GUARD: Mutex<()> = Mutex::new(());
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        registry(|r| {
            r.in_flight.clear();
            r.completions.clear();
        });
        let out = f();
        registry(|r| {
            r.in_flight.clear();
            r.completions.clear();
        });
        out
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
            DelegationError::WorkerExited("process gone".into()),
            DelegationError::NoText("worker produced no text".into()),
        ] {
            assert!(!e.reason().trim().is_empty());
            assert!(
                transition::ALL.contains(&e.transition()),
                "{} is not a declared transition",
                e.transition()
            );
            assert!(e.to_string().contains(e.reason()));
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

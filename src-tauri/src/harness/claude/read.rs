//! V20: Claude Code out-of-band TTS via transcript tail.
//!
//! Claude Code appends a JSONL transcript per session at
//! `~/.claude/projects/<slug>/<id>.jsonl`, where `<slug>` is the project cwd
//! with every `\`, `/`, and `:` replaced by `-`. Assistant lines look like
//! `{"type":"assistant","message":{"id":..,"content":[{"type":"text",..},
//! {"type":"thinking",..}]}}` and the `text` block is written **complete at
//! message finish** (block-level). We tail the newest `*.jsonl` in the project
//! dir, emit each new assistant `text` block to TTS, and skip `thinking` and
//! tool blocks.
//!
//! Latency is sub-second in practice (spike 0b), well within TTS comfort.
//!
//! ## Sub-agent transcripts (two contracts)
//!
//! Sub-agent traffic has lived in two places across CLI releases: inline in
//! the parent transcript as `isSidechain:true` lines (1.x), and — since the
//! 2.x contract (observed 2.1.207) — in per-agent files at
//! `<slug>/<session_id>/subagents/agent-<id>.jsonl`. The parent drain handles
//! the inline form; [`SubagentState`] tails the per-agent files, feeding ONLY
//! the usage and commit-provenance taps (a sub-agent's tokens/commits are the
//! parent session's spend; its reads, prompts, and text are not the parent's
//! working set, turn clocks, or TTS). `drift_tick` is the canary that fires
//! when this contract moves again.
//!
//! ## Format tolerance (the transcript is an UNSTABLE contract)
//!
//! Claude Code declares the transcript JSONL format unstable — it can change on
//! any release (2.1.212 added `effort` to assistant messages; 2.x renamed the
//! sub-agent launcher `Task` → `Agent`). Every reader in this module therefore
//! walks an untyped [`serde_json::Value`] with `get`/`as_str`, never a typed
//! serde struct or enum:
//!
//!  * **Unknown fields** are ignored by construction (no struct to reject them,
//!    and nothing here uses `deny_unknown_fields`).
//!  * **Unknown enum-ish values** — a new `type`, a new tool `name`, a new
//!    `source` variant — fall through the `match`/`==` arms as "not one of the
//!    shapes we act on", leaving the rest of the line's taps untouched instead
//!    of failing the whole line.
//!  * **Unparseable lines** are skipped and logged ([`parse_transcript_line`]);
//!    the tail keeps draining the lines after them.
//!
//! Keep new readers to that discipline: a typed struct here would turn an
//! upstream field addition into a dead tap.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, trace, warn};

use super::super::OobContext;
// V40 Phase G: the two lane ids this harness DECLARES, spelled once in
// `usage.rs` and written verbatim into `usage_stat.origin`.
use super::usage::{ORIGIN_AGENT, ORIGIN_SESSION};
use crate::state::StateSignal;

const POLL: Duration = Duration::from_millis(200);

/// H1-R2 (2026-08-05 review): how often [`TapHeartbeat`] re-stamps this tab's
/// registry entries. Comfortably inside `graph::service::LIVE_SESSION_TTL_MS`
/// (90 s) with room for several missed ticks — the heartbeat's only job is that
/// a drain loop parked in TTS backpressure can never let a claim age out.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// H1-R2: keeps a Claude tab's live-registry claims fresh **independently of the
/// drain loop**, and retires them in an order that cannot resurrect a cleared
/// entry.
///
/// *Why it exists.* The tap re-marks its `LiveTabRoot` (and its live session)
/// only at the top of [`run`]'s loop, but the loop can park for far longer than
/// the registry TTL inside `drain_new_lines` → [`OobContext::speak`], which
/// awaits a bounded TTS channel drained at synthesis speed. If tab A's claims
/// aged out, tab B would stop seeing a co-tenant, `tab_binding_is_ambiguous`
/// would answer `false`, and B's tap — tailing A's transcript, the newest file
/// under the shared root — would gain a CONFIDENT and WRONG session binding:
/// exactly the H1 symptom the ambiguity predicate exists to remove. A separate
/// task with no dependency on the drain's awaits is what makes the claim's
/// liveness a function of the TAB being open rather than of TTS throughput.
///
/// *Why the mutex is held across the marks.* `retire()` and the guard's
/// `clear_live_session()` must be totally ordered against a tick: see
/// [`Self::tick`].
#[derive(Default)]
struct TapHeartbeat {
    state: StdMutex<HeartbeatState>,
}

#[derive(Default)]
struct HeartbeatState {
    /// Set by [`TapHeartbeat::retire`] when the tap is going away. Once set, no
    /// tick may touch the registry again.
    retired: bool,
    /// The session id the drain loop last CONFIRMED live for this tab, if any
    /// (`None` until confirmation — the first transcript a tap attaches to is
    /// usually a finished session, and marking it live would report the wrong
    /// one; see [`LiveSessionGate`]).
    session: Option<String>,
    /// V34: whether the drain loop is still honouring its `--session-id` pin.
    /// Mirrored here so a heartbeat tick re-stamps the SAME pinned-ness the
    /// drain last claimed — a tick that asserted `pinned` after the drain gave
    /// up would re-arm a proof that no longer holds.
    pinned: bool,
}

impl TapHeartbeat {
    /// Record the session the drain loop just marked live, so subsequent ticks
    /// refresh `live_sessions` too — not only the root claim. Called from the
    /// same place (and under the same condition) as the loop's own
    /// `mark_live_session`, so the heartbeat can never invent a session the
    /// drain hasn't confirmed.
    fn note_session(&self, session_id: &str) {
        if let Ok(mut st) = self.state.lock() {
            if st.session.as_deref() != Some(session_id) {
                st.session = Some(session_id.to_string());
            }
        }
    }

    /// V34: seed the pin state, and later clear it if the drain gives up (the
    /// harness never wrote the pinned transcript). Same discipline as
    /// [`Self::note_session`] — the heartbeat only ever repeats a claim the
    /// drain loop actually holds.
    fn note_pinned(&self, pinned: bool) {
        if let Ok(mut st) = self.state.lock() {
            st.pinned = pinned;
        }
    }

    /// One heartbeat tick: re-stamp this tab's registry claims. Returns whether
    /// the heartbeat is still live — `false` means retired, and the caller must
    /// stop.
    ///
    /// **Ordering contract (load-bearing).** The marks run while holding
    /// `state`, and [`Self::retire`] takes the same lock to set `retired`. So a
    /// concurrent teardown either (a) wins the lock first, in which case this
    /// tick observes `retired` and marks nothing, or (b) waits until this tick's
    /// marks have completed, and only then does the guard's
    /// `clear_live_session()` run — removing whatever this tick just wrote. In
    /// neither interleaving can a tick land AFTER the clear and resurrect a
    /// dropped entry. (Aborting the task instead would not be enough: `abort`
    /// only takes effect at the next await point, so a tick already inside a
    /// mark could still finish after the clear.)
    ///
    /// No lock-order hazard: this path takes `state` → registry, while `retire`
    /// releases `state` before the guard touches the registry, so the two are
    /// never nested in opposite orders.
    fn tick(&self, ctx: &OobContext, root: &Path) -> bool {
        let Ok(st) = self.state.lock() else {
            return false; // poisoned ⇒ treat as retired; never mark blind.
        };
        if st.retired {
            return false;
        }
        ctx.mark_live_tab_root("claude", root, st.pinned);
        if let Some(sid) = st.session.as_deref() {
            ctx.mark_live_session(sid, "claude");
        }
        true
    }

    /// Stop all future ticks, and wait out any tick already in flight (the lock
    /// is the barrier). Must be called BEFORE the registry entries are cleared —
    /// see [`Self::tick`].
    fn retire(&self) {
        match self.state.lock() {
            Ok(mut st) => st.retired = true,
            // Poisoned by a panicking tick: `tick` treats poisoning as retired
            // too, so the invariant still holds.
            Err(mut e) => e.get_mut().retired = true,
        }
    }
}

/// V24 Phase B: RAII cleanup of a Claude tab's live-session registry entry.
/// Created once at the top of [`run`], it clears the entry (keyed by the stable
/// tab id, via [`OobContext::clear_live_session`]) on `Drop` — i.e. on every one
/// of `run`'s cancel/return paths — so a closed tab stops being reported active
/// without waiting for its TTL. Mirrors `service`'s other RAII guards.
///
/// H1 fix: the same call also drops this tab's `LiveTabRoot` claim, so closing
/// one of two same-project Claude tabs restores the survivor's session scoping
/// immediately instead of after the registry TTL.
///
/// H1-R2: it also owns the retirement of the tap's [`TapHeartbeat`]. The order
/// inside `drop` is load-bearing — retire FIRST, clear SECOND — so the last
/// possible heartbeat write happens before the clear that must outlive it.
struct LiveSessionGuard<'a> {
    ctx: &'a OobContext,
    hb: Arc<TapHeartbeat>,
}

impl Drop for LiveSessionGuard<'_> {
    fn drop(&mut self) {
        // ORDER IS LOAD-BEARING (H1-R2): retiring the heartbeat both stops
        // future ticks and blocks until any in-flight tick's marks are done, so
        // nothing can re-mark this tab after the clear below and resurrect a
        // claim the closed tab no longer owns (a phantom co-tenant would
        // suppress the survivor's scoping for a full TTL). Never reorder these.
        self.hb.retire();
        self.ctx.clear_live_session();
    }
}

/// H1-R2: run `hb`'s ticks on their own task, tied to the same cancel token as
/// the tap, so no `await` on the drain path can starve the registry refresh.
/// The task holds only a clone of the tap's context (tab id + registry handle +
/// cancel token) and the root; it never reads the transcript.
fn spawn_heartbeat(ctx: OobContext, root: PathBuf, hb: Arc<TapHeartbeat>) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => return,
                _ = sleep(HEARTBEAT) => {}
            }
            if !hb.tick(&ctx, &root) {
                return; // retired: the tap's guard has run (or is running).
            }
        }
    });
}

/// Whether the transcript the tap is attached to is demonstrably **this tab's
/// live session** — the one fact that may repoint the live-session registry.
///
/// # Why this is a type and not a `bool` in [`run`]
///
/// It was a `bool`, and its rule was `live_confirmed = !first_attach`: a file
/// that appeared after launch was taken as a freshly-started session, live *by
/// construction*, with no growth check. The 2026-08-07 review's finding C-2 is
/// what that costs, and the chain runs through three modules, which is why the
/// decision now has a name and a test rather than living inside a loop:
///
/// 1. A Claude tab's session id is the file stem of the newest `*.jsonl` in
///    `~/.claude/projects/<encoded-root>/`, and [`newest_jsonl`] ranks purely by
///    mtime. So `type nul > …/aaaa.jsonl` from Bash — a *zero-byte* file — wins
///    the ranking within one 200 ms poll.
/// 2. The old rule marked it live immediately, and `mark_live_session` repoints
///    the registry entry keyed by this tab id.
/// 3. `loopback::TabLatch::observe` reads that entry, sees a **changed** session
///    id, and treats it as a new conversation: `latch = Open`, `budget.reset()`,
///    `latch_flagged = false` — and, until H-2, **`contaminated = false`**. It
///    is called from all three state paths (`gate`, `beacon`, `view_for`), so
///    even a `/latch/state` poll applies it.
///
/// # The rule, and what H-2 changed about it
///
/// The rule was **"growth is the proof, and it is the only proof"**: a file's
/// appearance means nothing, bytes arriving in it mean a live harness is
/// writing. The 2026-08-08 re-review's H-2 is what that costs: growth proves
/// something is writing, not that the *harness* is writing, and
/// `read_complete_lines` advances the offset for any newline-terminated bytes.
/// `echo {} > <newest>.jsonl` cleared the bar in one command.
///
/// So the bar here is now a **decode proof**: at least one line of the new bytes
/// must parse as a transcript record AND carry a top-level `sessionId` equal to
/// this session's id (see `record_names_session`). That is strictly stronger
/// than an offset delta, and it is what [`observed`](Self::observed) takes.
///
/// **It is defence in depth, not a trust root, and the difference matters.** The
/// attacker who creates the file also writes its contents, so
/// `echo '{"sessionId":"aaaa"}' > aaaa.jsonl` still clears this bar — decision 3
/// puts Claude's native Bash outside every cImp latch. Nothing derived from the
/// transcript directory can be a trust root for the *sharp* consequence, so the
/// sharp consequence no longer hangs off it: `TabLatch::contaminated` is now
/// sticky and a rotation cannot clear it, whatever this gate says. What this
/// gate still protects is the live-session registry itself — session-scoped
/// memory (V28), the Usage "active now" set, and permission attribution — where
/// a wrong answer is a misattribution rather than a released containment bit.
///
/// Cost: a genuinely new session is reported live one poll (200 ms) later than
/// before, once its first line lands — the harness writes its first entry within
/// the same tick it creates the file, so this is not observable in practice.
///
/// This is the filesystem half only. The token half — a `/memory/event` POST
/// keying the same registry with a tab-colliding string — is closed in
/// `offload/loopback.rs::mark_live_session_from_event`. **Neither alone is
/// sufficient**: they are two independent writers into one registry.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveSessionGate {
    confirmed: bool,
}

impl LiveSessionGate {
    /// The tap attached to a different transcript file. Confirmation is
    /// dropped, unconditionally — a file that merely *appeared* is not evidence
    /// of a conversation, whoever created it.
    pub(crate) fn rotated(&mut self) {
        self.confirmed = false;
    }

    /// Fold one drain's evidence in. `own_record` is
    /// [`Drained::own_record`] — "the bytes just consumed contained at least one
    /// decoded transcript record naming this session". Confirmation is then
    /// permanent until the next rotation (a quiet turn must not un-confirm a
    /// session already proved). Returns whether the tap may report this session
    /// live.
    ///
    /// H-2: this deliberately takes the *evidence*, not `(before, after)`
    /// offsets. The old signature made "the offset moved" the predicate, and an
    /// offset moves for any newline-terminated byte — including one written by
    /// the model's own shell.
    pub(crate) fn observed(&mut self, own_record: bool) -> bool {
        if own_record {
            self.confirmed = true;
        }
        self.confirmed
    }
}

/// Tail the active transcript for `project_dir`, speaking new assistant text
/// until the tab's cancel token fires. Resilient: if the project dir or any
/// file is missing it simply waits; transient read/parse errors are skipped.
pub async fn run(project_dir: PathBuf, pinned_session: Option<String>, ctx: OobContext) {
    let root = match project_root(&project_dir) {
        Some(r) => r,
        None => {
            debug!(tab = ?ctx.tab, "Claude OOB: no home dir; transcript tail disabled");
            return;
        }
    };
    debug!(tab = ?ctx.tab, root = %root.display(), "Claude OOB: watching transcripts");

    // V24 Phase B: keep this tab's live-session registry entry current while the
    // tail runs, and clear it (via the guard's `Drop`) on any exit path — tab
    // cancel or the source ending — so a closed tab drops out of the "active
    // now" set before its TTL lapses.
    //
    // H1-R2: the guard also owns the heartbeat that keeps those entries fresh
    // while this loop is blocked (see `TapHeartbeat`). Constructed BEFORE the
    // task is spawned so no tick can run without a guard that will retire it.
    let hb = Arc::new(TapHeartbeat::default());
    let _live_guard = LiveSessionGuard {
        ctx: &ctx,
        hb: hb.clone(),
    };
    spawn_heartbeat(ctx.clone(), root.clone(), hb.clone());

    let mut seen: HashSet<String> = HashSet::new();
    // V39 review HIGH-1: the delegation completion is per TURN, and a turn
    // outlives one drain pass — so its buffer lives here, beside `seen`, for
    // the lifetime of this tap.
    let mut turn = TurnText::default();
    // Tool-use IDs of `Task` sub-agents launched but not yet resolved. Non-
    // empty ⇒ at least one agent is running, which holds the avatar in Thinking
    // (see `update_agents`). Keyed by the `toolu_…` id so out-of-order results
    // and parallel launches are matched exactly.
    let mut agents: HashSet<String> = HashSet::new();
    // V14 Phase C: tool_use_id -> name, so a later tool_result can be
    // attributed to the tool that produced it for usage accounting. Same
    // per-session lifetime as `agents` — cleared on session rotation below.
    let mut tool_names = ToolNameRing::default();
    // Session→commit provenance: tool_use ids whose command is a `git
    // commit` invocation, awaiting their result (see `record_commit_events`).
    // Same per-session lifetime as `tool_names`.
    let mut commit_calls = IdRing::default();
    // Sub-agent transcript tails + the drift canary state (see module doc).
    // Same per-session lifetime as the rings — reset on rotation below.
    let mut subs = SubagentState::default();
    let mut cur: Option<PathBuf> = None;
    let mut offset: u64 = 0;
    // V16 Feature 1: capture the Claude CLI version (each transcript entry
    // carries a top-level `version` field) at most once per session file —
    // the write is change-guarded downstream, this flag just avoids re-parsing
    // for it on every line.
    let mut version_noted = false;
    // The first file we attach to may already hold a long backlog from before
    // launch; skip it by seeking to EOF. Files that appear *later* (a new
    // session) are read from the start.
    let mut first_attach = true;
    // Whether the attached transcript is demonstrably THIS tab's live session
    // — a decoded record naming the session is the proof, on every attach and
    // every rotation alike (H-2). See [`LiveSessionGate`].
    let mut live = LiveSessionGate::default();
    // V34: the session id cImp pinned on this child's command line, while we
    // are still honouring it. Taken by value so `pin_step`'s `GiveUp` can clear
    // it permanently for this tap. `pin_since` dates the grace window from the
    // tap's start, not from the tab's.
    // The session id cImp REQUESTED for this tab. Held for the tap's lifetime
    // (never cleared) because a conversation can start writing at any point, so
    // a pin unverified now may verify later. Nothing is published from it until
    // its transcript exists — see [`PinStep`].
    let pin = pinned_session;
    // Latched once the pinned transcript is seen: this tab's identity is
    // PROVEN, and it is that fact — not the presence of a `--session-id` on the
    // argv — that the registry's ambiguity exemption keys off.
    let mut pin_verified = false;
    // One-shot so the "pin not honoured" note doesn't repeat every 200ms tick.
    let mut pin_unhonoured_logged = false;

    loop {
        if ctx.cancel.is_cancelled() {
            return;
        }

        // H1 fix (2026-08-05 review): declare this tab's transcript ROOT while
        // the tap runs. `newest_jsonl(&root)` below has no per-process
        // discriminator, so a second running Claude tab on the same project
        // binds to the same file and every tab-keyed identity claim from either
        // tab becomes unprovable; the registry uses this map to detect exactly
        // that and degrade to unscoped/unattributed instead of guessing (see
        // `graph::service::tab_binding_is_ambiguous`). Marked BEFORE the first
        // `newest_jsonl` so a tab is a known co-tenant from its tap's first
        // instruction, not from its first confirmed session — that is what
        // closes the launch-order window. Cleared by `_live_guard` on exit.
        // V34: a VERIFIED pinned tab follows exactly one file — the transcript
        // named by the `--session-id` cImp put on this child's argv — so the
        // "newest wins" race above does not apply to it. A tab whose pin the
        // harness did not honour (a restored conversation keeps its original
        // session id) is indistinguishable from an unpinned one, and is treated
        // as such: same file choice, same ambiguity rules, no identity claim.
        let target = match pin.as_deref() {
            Some(sid) => {
                let path = root.join(format!("{sid}.jsonl"));
                match pin_step(path.is_file()) {
                    PinStep::Follow => {
                        // Verified: the harness wrote the transcript we asked
                        // for. Claim the identity — and because WE chose the id,
                        // this claim needs no further proof from the file's
                        // contents (the `LiveSessionGate` bar exists for the
                        // newest-wins path, where the file's ownership is the
                        // very thing in doubt).
                        if !pin_verified {
                            pin_verified = true;
                            debug!(tab = ?ctx.tab, session = %sid, "Claude OOB: pin verified");
                        }
                        hb.note_pinned(true);
                        hb.note_session(sid);
                        ctx.mark_live_session(sid, "claude");
                        Some(path)
                    }
                    PinStep::Fallback => {
                        // Log ONCE, not per tick: for a restored conversation
                        // this is the steady state, not an error.
                        if !pin_unhonoured_logged {
                            pin_unhonoured_logged = true;
                            debug!(
                                tab = ?ctx.tab,
                                session = %sid,
                                "Claude OOB: no transcript for the pinned session yet — \
                                 running unpinned (a restored conversation keeps its own id)"
                            );
                        }
                        hb.note_pinned(false);
                        newest_jsonl(&root)
                    }
                }
            }
            None => {
                hb.note_pinned(false);
                newest_jsonl(&root)
            }
        };
        // Reported from the VERIFIED state, never from "we passed the flag".
        ctx.mark_live_tab_root("claude", &root, pin_verified);

        match target {
            Some(path) if Some(&path) != cur.as_ref() => {
                // Rotated to a new (or first) transcript file. Either way the
                // file must prove itself by yielding a DECODED record that
                // names its session before it is reported live (V32 C-2, then
                // H-2 — see [`LiveSessionGate`]); `first_attach` still decides
                // the backlog posture, and only that.
                live.rotated();
                offset = if first_attach {
                    std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                // Sub-agent files inherit the same backlog posture: attached
                // mid-session ⇒ pre-existing agent transcripts seek to EOF.
                subs.reset(first_attach);
                first_attach = false;
                cur = Some(path);
                // Any agents we were tracking belonged to the previous session
                // file; a new file is a new session. Clear and, if we had
                // announced "agents active", release the avatar so it can't
                // wedge in Thinking across the rotation.
                if !agents.is_empty() {
                    agents.clear();
                    // V35 Phase L: arbitrated with the edge in `update_agents`,
                    // and it has to be. For a tab whose sub-agent lifecycle is
                    // PUSHED, a rotation says nothing about whether an agent is
                    // running — releasing the avatar here would contradict a
                    // `SubagentStart` that has not been stopped.
                    if !ctx.pushed("claude", crate::harness::chp::EV_SESSION_SUBAGENT) {
                        ctx.signal(StateSignal::SubagentsActiveChanged {
                            tab: ctx.tab.clone(),
                            active: false,
                        });
                    }
                }
                // The tool-name ring is per-session too: a new file means old
                // tool_use ids can never see a matching tool_result.
                tool_names.clear();
                commit_calls.clear();
                version_noted = false;
            }
            Some(_) => {}
            None => {
                // No transcript yet; wait for one to appear.
                tokio::select! {
                    _ = ctx.cancel.cancelled() => return,
                    _ = sleep(POLL) => continue,
                }
            }
        }

        if let Some(path) = cur.clone() {
            // The transcript filename stem is the Claude session id — the memory
            // scope key. `<id>.jsonl` → `<id>`.
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let drained = drain_new_lines(
                &path,
                offset,
                &mut seen,
                &mut turn,
                &mut agents,
                &mut tool_names,
                &mut commit_calls,
                &mut version_noted,
                &mut subs,
                &project_dir,
                &session_id,
                &ctx,
            )
            .await;
            offset = drained.offset;
            // V24 Phase B: refresh this tab's live-session registry entry
            // (keyed by the stable tab id) so the Usage snapshot marks the
            // session active — but only once the file is confirmed to be this
            // tab's own session, so a dead previous-run transcript is never
            // reported as the current one.
            //
            // H-2: the gate takes the drain's DECODE evidence, never the offset
            // delta — the offset above advances for any newline-terminated byte.
            if live.observed(drained.own_record) {
                ctx.mark_live_session(&session_id, "claude");
                // H1-R2: hand the confirmed session to the heartbeat so it
                // refreshes `live_sessions` too while this loop is parked in
                // TTS backpressure — a live root claim with a lapsed session
                // entry would still lose the tab its permission attribution.
                hb.note_session(&session_id);
            }
            // Sub-agent transcripts (2.x contract): drain usage/commits from
            // `<sid>/subagents/*.jsonl`, then tick the drift canary. Both are
            // mem-gated inside — pure TTS setups skip the extra IO entirely.
            subs.scan(&root, &session_id, &project_dir, &ctx);
            subs.drift_tick(&project_dir, &session_id, &ctx);
        }

        tokio::select! {
            _ = ctx.cancel.cancelled() => return,
            _ = sleep(POLL) => {}
        }
    }
}

/// What one [`drain_new_lines`] pass consumed — the two facts H-2 requires be
/// kept apart.
///
/// The offset is bookkeeping: it advances past every complete line so the tail
/// never re-reads or skips, and it must go on doing exactly that whatever the
/// lines contained. The evidence flag is a *claim about identity*, and it is set
/// only by a line that decoded and named this session. Deriving the second from
/// the first is the H-2 defect: `read_complete_lines` moves the offset for any
/// newline-terminated bytes, so a one-byte `echo` was indistinguishable from a
/// live harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drained {
    /// The new tail offset — past whole lines only, partial trailing line held
    /// back (see [`read_complete_lines`]).
    offset: u64,
    /// At least one line in this chunk parsed AND carried a top-level
    /// `sessionId` equal to the `session_id` the drain was called with. Fed to
    /// [`LiveSessionGate::observed`]; nothing else may set it.
    own_record: bool,
}

/// Whether a decoded transcript line is a record **of** `session_id`: a
/// top-level `"sessionId"` equal to it.
///
/// H-2's predicate, isolated so it has a name and a test. Three deliberate
/// properties:
///
/// * **Not every line has the field** — `{"type":"file-history-snapshot",…}` has
///   no `sessionId`. Such a line is simply *not evidence*: it neither confirms
///   nor vetoes, so a real transcript that opens with one is confirmed by its
///   next line instead.
/// * **An empty `session_id` never matches.** The id is a `file_stem` that falls
///   back to `""`; a bare `.jsonl` must not be confirmable by a line carrying
///   `"sessionId":""`.
/// * **Untyped `get`**, per the module's format-tolerance discipline — a
///   non-object line (`3`, `[]`) answers `false` rather than panicking.
///
/// `pub(crate)` for the V35 Phase D live probe (`harness/probe.rs`), which
/// proves `claude.transcript.identity` through this exact predicate rather than
/// re-deriving "does a line name its own session".
pub(crate) fn record_names_session(obj: &Value, session_id: &str) -> bool {
    !session_id.is_empty() && obj.get("sessionId").and_then(Value::as_str) == Some(session_id)
}

/// **V39 review HIGH-1 — the current turn's answer, and whether the turn is
/// over.**
///
/// The delegation completion feed is per TURN, not per message
/// (`delegation::note_assistant_text`): a mid-turn preamble filed as the reply
/// releases the worker's slot while it is still working. The pushed side gets
/// that for free — the `Stop` hook fires once, with `last_assistant_message` —
/// so this is the fallback reader's half of the same contract.
///
/// # The turn boundary, and what it is derived from
///
/// A transcript assistant line carries `message.stop_reason`, and it is the
/// only turn-shaped fact in the file: `"tool_use"` means the model stopped to
/// call a tool and the turn continues; anything else non-null (`"end_turn"`,
/// and the rarer `"max_tokens"` / `"stop_sequence"` / a value a future build
/// adds) means it stopped talking. Unknown values therefore END a turn, which
/// is the fail-toward-answering direction: the worst case is filing a
/// completion one message early, never a delegation that hangs to its deadline
/// with the answer sitting unread on screen.
///
/// # Why the fire happens at the END of a drain pass
///
/// One API message is written as SEVERAL transcript lines — one per content
/// block, all carrying the same `stop_reason` — and the order is the content's
/// (`thinking`, then `text`). Firing on the first `end_turn` line would file
/// whatever text preceded the thinking block. So the pass buffers, marks the
/// turn ended, and files once when the pass is done. A turn split across two
/// poll ticks still files: `ended` is sticky until the next user prompt, so
/// the text arriving in the following pass fires it.
#[derive(Debug, Default)]
struct TurnText {
    /// The last non-sidechain assistant text seen in this turn.
    text: Option<String>,
    /// A terminal `stop_reason` has been seen for this turn.
    ended: bool,
    /// This turn's completion has been filed. One turn, one completion.
    fired: bool,
    /// Drain passes observed since the turn ended with no text yet (V39 review
    /// R-9). One pass of grace before an empty completion is filed — see
    /// [`Self::take_if_over`].
    passes_since_end: u8,
}

impl TurnText {
    /// A genuine user prompt starts a new turn — drop everything the previous
    /// one buffered, so a stale answer can never be filed as the new turn's.
    fn restart(&mut self) {
        *self = Self::default();
    }

    /// Fold one non-sidechain assistant line in.
    fn note_line(&mut self, obj: &Value, texts: &[String]) {
        if let Some(last) = texts.last() {
            self.text = Some(last.clone());
        }
        if is_turn_end(obj) {
            self.ended = true;
        }
    }

    /// The completion to file now, if the turn is over.
    ///
    /// **A turn that ended with NO text files an EMPTY completion** (V39 review
    /// R-9, locked decision 13: empty is not absent). The engine turns an empty
    /// completion into `NoText` — "the worker finished its turn without a
    /// substantive final message" — immediately, which is the honest answer;
    /// filing nothing made the same outcome arrive as a `timeout` ten minutes
    /// later, saying the worker was still running when it had stopped.
    ///
    /// `definitive` is what stops that from firing early. One API message is
    /// several transcript lines carrying one `stop_reason`, and the `thinking`
    /// line comes BEFORE the `text` one — so at the end of the pass that first
    /// saw `end_turn`, "no text yet" and "no text at all" are the same picture.
    /// A pass of grace tells them apart. At a user prompt (`definitive`) there
    /// is nothing left to wait for: the next turn has started.
    fn take_if_over(&mut self, definitive: bool) -> Option<String> {
        if !self.ended || self.fired {
            return None;
        }
        if let Some(text) = self.text.clone() {
            self.fired = true;
            return Some(text);
        }
        self.passes_since_end = self.passes_since_end.saturating_add(1);
        if !definitive && self.passes_since_end < 2 {
            return None;
        }
        self.fired = true;
        Some(String::new())
    }
}

/// Whether this transcript line ends its turn — see [`TurnText`].
///
/// `pub(crate)` for the V35 canary suite: this is the one reader behind
/// `claude.transcript.stop_reason`, and a canary that re-implemented the rule
/// would prove its own copy rather than the code that runs.
pub(crate) fn is_turn_end(obj: &Value) -> bool {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    match obj
        .get("message")
        .and_then(|m| m.get("stop_reason"))
        .and_then(Value::as_str)
    {
        // The model paused to call a tool: the turn continues.
        Some("tool_use") => false,
        Some(other) => !other.trim().is_empty(),
        // Absent or null — nothing is claimed either way.
        None => false,
    }
}

/// Whether a transcript line is a sub-agent's own message (the 1.x inline
/// contract). Those are not the tab's turn and must never complete one.
fn is_sidechain(obj: &Value) -> bool {
    obj.get("isSidechain").and_then(Value::as_bool) == Some(true)
}

/// Read complete new lines from `path` starting at `offset`, speaking assistant
/// text, and return the new offset (advanced only past whole lines) together
/// with whether any of those lines identified this session ([`Drained`]).
#[allow(clippy::too_many_arguments)]
async fn drain_new_lines(
    path: &Path,
    mut offset: u64,
    seen: &mut HashSet<String>,
    // V39 review HIGH-1: the delegation completion's turn buffer, owned by
    // `run` because a turn outlives one drain pass.
    turn: &mut TurnText,
    agents: &mut HashSet<String>,
    tool_names: &mut ToolNameRing,
    commit_calls: &mut IdRing,
    version_noted: &mut bool,
    subs: &mut SubagentState,
    project_dir: &Path,
    session_id: &str,
    ctx: &OobContext,
) -> Drained {
    // H-2: the offset advance is unconditional and unchanged — a chunk with no
    // evidence in it is still fully consumed. Only `own_record` is earned.
    let mut own_record = false;
    let Some((complete, new_offset)) = read_complete_lines(path, offset) else {
        // Nothing new/complete, or rotated away mid-loop.
        //
        // V39 review R-9: an empty pass is still a pass. A turn that ended with
        // no text is waiting out one pass of grace (see `TurnText`), and if the
        // file has gone quiet — which is exactly what a finished turn looks
        // like — this is the pass that ends the wait.
        if let Some(text) = turn.take_if_over(false) {
            ctx.note_turn_text(&text);
        }
        return Drained { offset, own_record };
    };
    offset = new_offset;

    for line in complete.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(obj) = parse_transcript_line(path, line) {
            // H-2: the ONE place the live-session evidence flag is set. Inside
            // the parse arm by construction, so an unparseable line can never
            // be evidence — and outside every early `continue`, so the drain's
            // "skip the bad line, keep draining" posture is untouched.
            own_record |= record_names_session(&obj, session_id);
            note_cli_version(&obj, version_noted);
            // Canary fact: an inline sidechain line means the 1.x sub-agent
            // contract is (still) live — see `SubagentState::drift_tick`.
            if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                subs.sidechain_seen = true;
            }
            let delta = update_agents(&obj, agents, ctx);
            subs.launch_seen |= delta.launched;
            subs.completion_seen |= delta.completed;
            note_user_turn(&obj, session_id, ctx);
            record_tool_events(&obj, project_dir, session_id, ctx);
            // Parent transcript: Session by default; `record_usage` upgrades an
            // inline `isSidechain:true` line to Agent (1.x sub-agent contract).
            record_usage(
                &obj,
                tool_names,
                project_dir,
                session_id,
                ctx,
                ORIGIN_SESSION,
            );
            record_commit_events(&obj, commit_calls, project_dir, session_id, ctx);
            // V35 Phase L — **arbitration, and why the dedup set is fed
            // anyway.** A tab whose `Stop` hook pushes `assistant_text` has
            // this tap suppressed (locked decision 4: push wins when served).
            // `seen` is still filled, because a tab can start serving
            // mid-session — `SessionStart` fires on `resume` and `clear` — and
            // a `seen` set with a hole in it would re-speak everything the
            // suppressed window covered if the hello were later retired by a
            // relaunch. The handoff in `tts::prose` covers the other half of
            // that boundary: the first push after a switchover strips whatever
            // this tap already spoke of the same message.
            let pushed = ctx.pushed("claude", crate::harness::chp::EV_ASSISTANT_TEXT);
            // V39 review HIGH-1: a genuine user prompt is the start of a turn,
            // and the previous turn's buffer must not survive it.
            //
            // **Review R-2: file the ended turn FIRST.** The fire is deferred
            // to the end of the pass (an API message is several lines, and the
            // final text can follow the line that declared the turn over), so a
            // pass that carried both a turn's end AND the user's next prompt —
            // routine when the user types quickly, or when a paused tap catches
            // up — wiped an ended-but-unfiled turn and the delegation waiting on
            // it ran to its deadline instead of completing. The boundary is
            // here, not at the end of the pass, precisely because the prompt is
            // what makes the previous turn unambiguously over.
            if !pushed && is_user_prompt(&obj) && !is_sidechain(&obj) && obj.get("isMeta").and_then(Value::as_bool) != Some(true) {
                // Definitive: the next turn has started, so an ended turn
                // with no text is an empty answer NOW rather than after a pass
                // of grace.
                if let Some(text) = turn.take_if_over(true) {
                    ctx.note_turn_text(&text);
                }
                turn.restart();
            }
            let mut fresh: Vec<String> = Vec::new();
            for (key, text) in assistant_texts(&obj) {
                if seen.insert(key) && !pushed {
                    trace!(tab = ?ctx.tab, "Claude OOB: speaking assistant block");
                    fresh.push(text.clone());
                    ctx.speak(&text).await;
                }
            }
            // V39 Phase B + review HIGH-1: the same arbitrated branch feeds
            // delegation's completion signal — but per TURN, buffered here and
            // filed below. Sidechain lines are a sub-agent's own messages and
            // are never the tab's turn. TTS is untouched: it speaks each block
            // as it lands, which is what makes speech track the tab.
            if !pushed && !is_sidechain(&obj) {
                turn.note_line(&obj, &fresh);
            }
        }
    }
    // Once per pass, after every line of it — an API message is several
    // transcript lines carrying one `stop_reason`, so the turn's final text can
    // follow the line that declared the turn over.
    if let Some(text) = turn.take_if_over(false) {
        ctx.note_turn_text(&text);
    }
    Drained { offset, own_record }
}

/// The CLI build that wrote a transcript line — the top-level `version` field,
/// trimmed, and `None` when absent OR blank (global principle 5: an empty
/// version string is not a version).
///
/// One reader, two consumers: [`note_cli_version`] feeds the V16
/// `harness_versions` tripwire from it, and the V35 Phase D live probe reads it
/// to prove `claude.transcript.identity` still carries a build stamp — the row
/// whose loss *silences* the tripwire rather than firing it, so the probe must
/// not go through the recording path (it must never write).
/// `pub(crate)` for `harness/probe.rs`.
pub(crate) fn cli_version_of(obj: &Value) -> Option<&str> {
    obj.get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// V16 Feature 1: record the Claude Code CLI version from a transcript entry's
/// top-level `version` field into the global `harness_versions` tripwire state.
/// Once per session file (`noted` flips on the first line that carries one);
/// the actual disk write is additionally change-guarded in
/// `note_harness_version`, and runs on a blocking thread — this is called from
/// the async tail loop.
fn note_cli_version(obj: &Value, noted: &mut bool) {
    if *noted {
        return;
    }
    let Some(v) = cli_version_of(obj) else {
        return;
    };
    *noted = true;
    let v = v.to_string();
    tokio::task::spawn_blocking(move || crate::settings::note_harness_version("claude", &v));
}

/// The `tool_use` names Claude Code emits when it launches a sub-agent:
/// `"Task"` through the 1.x CLIs, `"Agent"` since the 2.x transcript contract
/// (observed 2.1.207) — both matched so either vintage works. Keyed as a
/// named constant so the one dependency on these strings is greppable if a
/// future release renames the tool again; `SubagentState::drift_tick` is the
/// canary that catches such a rename in the wild (transcript files present
/// with no recognized launch).
const AGENT_TOOL_NAMES: &[&str] = &["Task", "Agent"];

/// The content-block array of a transcript line's `message`, or `None` when the
/// line has no array content (a plain-string user prompt, or a non-message
/// line). Shared by `assistant_texts` and `update_agents` so the
/// `message.content[]` shape is unwrapped in exactly one place.
/// `pub(crate)` for the V35 canary suite (harness/canary.rs).
pub(crate) fn message_parts(obj: &Value) -> Option<&Vec<Value>> {
    obj.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
}

/// Extract `(dedup_key, text)` for each assistant `text` block in a transcript
/// line. `thinking` and tool blocks are skipped. The key is `messageID` +
/// content prefix so a re-read (rotation/compaction) doesn't re-speak.
pub(crate) fn assistant_texts(obj: &Value) -> Vec<(String, String)> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let mid = obj
        .get("message")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut out = Vec::new();
    if let Some(parts) = message_parts(obj) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let prefix: String = text.chars().take(40).collect();
                out.push((format!("{mid}:{prefix}"), text.to_string()));
            }
        }
    }
    out
}

/// True when `obj` is a genuine user prompt — a `type:"user"` line whose content
/// is plain text (a string, or an array carrying a non-`tool_result` block)
/// rather than a tool-result carrier. Such a line is a turn boundary: the prior
/// turn is over, so any still-tracked `Task` ids (e.g. orphaned by an
/// Esc-interrupt that never wrote their `tool_result`) can be reclaimed.
fn is_user_prompt(obj: &Value) -> bool {
    if obj.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match obj.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(parts)) => parts
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) != Some("tool_result")),
        _ => false,
    }
}

/// V16 review fix: forward a genuine user prompt to the graph service as a
/// turn boundary for the read advisor's trust-TTL / compounding clocks —
/// with context injection off, `retrieve_context` never runs and nothing
/// else ticks `InjectState.turn` (the service no-ops when injection is on,
/// so the two clocks can't double-count). Sidechain lines (a sub-agent's
/// internal prompts) and `isMeta` lines (harness-inserted user messages —
/// local-command output, caveats) are not turns.
fn note_user_turn(obj: &Value, session_id: &str, ctx: &OobContext) {
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || obj.get("isMeta").and_then(Value::as_bool) == Some(true)
        || !is_user_prompt(obj)
    {
        return;
    }
    ctx.note_user_turn(session_id);
}

/// What one transcript line did to the tracked sub-agent set — the canary
/// facts `drain_new_lines` folds into [`SubagentState`].
#[derive(Default, Clone, Copy)]
struct AgentDelta {
    /// At least one sub-agent launch (`tool_use` named per
    /// [`AGENT_TOOL_NAMES`]) was tracked from this line.
    launched: bool,
    /// At least one `tool_result` cleared a TRACKED launch id — a genuine
    /// agent completion (turn-boundary reclaims of orphaned ids don't count).
    completed: bool,
}

/// Update the in-flight sub-agent set from one transcript line and emit
/// `SubagentsActiveChanged` when the running count crosses the zero boundary.
/// Returns the line's [`AgentDelta`] for the sub-agent drift canary.
///
/// An agent launch is a `tool_use` block named per [`AGENT_TOOL_NAMES`] (in
/// an assistant message); its completion is a `tool_result` block whose
/// `tool_use_id` matches (in the following user message). `agents` holds only
/// launch ids, so removing another tool's `tool_use_id` is a harmless no-op —
/// we don't need to know which tool a result belongs to, only whether it
/// clears a tracked agent.
///
/// Sidechain lines (a sub-agent's own internal messages, `isSidechain:true`) are
/// skipped so a nested tool_use/result inside an agent can't perturb the parent
/// count. The empty↔non-empty edge is all the state machine needs: parallel
/// launches in one message flip active once, and only the last result flips it
/// back.
///
/// A genuine new user prompt ([`is_user_prompt`]) is treated as a turn boundary
/// that clears the whole set: an Esc-interrupt can abort an agent without ever
/// writing its `tool_result`, so without this a stale id would keep the avatar
/// wedged in Thinking until the process exits. (The state manager also has a
/// time-based backstop for the walk-away case.)
fn update_agents(obj: &Value, agents: &mut HashSet<String>, ctx: &OobContext) -> AgentDelta {
    let mut delta = AgentDelta::default();
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return delta;
    }

    let was_active = !agents.is_empty();
    if is_user_prompt(obj) {
        agents.clear();
    } else if let Some(parts) = message_parts(obj) {
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("tool_use")
                    if part
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| AGENT_TOOL_NAMES.contains(&n)) =>
                {
                    if let Some(id) = part.get("id").and_then(Value::as_str) {
                        agents.insert(id.to_string());
                        delta.launched = true;
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = part.get("tool_use_id").and_then(Value::as_str) {
                        if agents.remove(id) {
                            delta.completed = true;
                        }
                    }
                }
                _ => {}
            }
        }
    } else {
        return delta;
    }

    let now_active = !agents.is_empty();
    // V35 Phase L — arbitration, on the SIGNAL only.
    //
    // The bookkeeping above (the `agents` set, and the `launched`/`completed`
    // deltas this returns) keeps running for a tab that pushes, and that is not
    // an oversight: those two flags feed `SubagentState::drift_condition`,
    // whose "the launcher tool was renamed" arm reads `!launch_seen`. Suppress
    // the bookkeeping and that canary starts firing on every session with a
    // sub-agent — a migration manufacturing the very false alarm it was meant
    // to make unnecessary.
    //
    // What is suppressed is the one thing that would be DOUBLE-delivered: the
    // avatar edge, which `SubagentStart`/`SubagentStop` now drive directly for
    // a serving tab.
    if now_active != was_active && !ctx.pushed("claude", crate::harness::chp::EV_SESSION_SUBAGENT) {
        debug!(tab = ?ctx.tab, count = agents.len(), active = now_active, "Claude OOB: agents active edge");
        ctx.signal(StateSignal::SubagentsActiveChanged {
            tab: ctx.tab.clone(),
            active: now_active,
        });
    }
    delta
}

/// V10: record file/query memory events from an assistant line's `tool_use`
/// blocks. Maps Claude's tool names → a memory `kind` + target
/// ([`super::tools::claude_memory_kind`]); tools not in that map (Task, TodoWrite,
/// our own `mcp__cimp-offload__*`) are ignored. Sidechain (sub-agent) lines are
/// skipped so an agent's internal reads don't pollute the parent session. A
/// no-op when memory isn't wired.
fn record_tool_events(obj: &Value, project_dir: &Path, session_id: &str, ctx: &OobContext) {
    if ctx.mem.is_none() {
        return;
    }
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(parts) = message_parts(obj) else {
        return;
    };
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = part.get("name").and_then(Value::as_str) else {
            continue;
        };
        // V16 Feature 4: every Bash command is also tested against the
        // session's recent read-advisor reminders — a `cat`/`Get-Content`
        // of a just-reminded file is the advisor's blind spot, and this tap
        // already sees the full command string for free.
        if name == "Bash" {
            if let Some(cmd) = part
                .get("input")
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str)
            {
                ctx.check_bypass(project_dir, session_id, cmd);
            }
        }
        let Some((kind, arg)) = super::tools::claude_memory_kind(name) else {
            continue;
        };
        let Some((path, detail)) = mem_target(arg, part.get("input")) else {
            continue;
        };
        ctx.record_mem(
            project_dir,
            session_id,
            "claude",
            kind,
            &path,
            None,
            None,
            detail.as_deref(),
        );
    }
}

/// Extract a classified tool's recordable target from its `input` args, or
/// `None` when the event carries nothing worth recording — a read/edit/grep
/// with no path, or a shell call with no `command` (recording those would
/// only evict useful events from the per-session ring). Mirrors the OpenCode
/// ingress guard in `offload::loopback::handle_memory_event` so both taps
/// classify identically.
fn mem_target(
    arg: crate::harness::plugin::MemArg,
    input: Option<&Value>,
) -> Option<(String, Option<String>)> {
    let get = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
    let (path, detail) = match arg {
        // Read/Edit key the target as `file_path`; NotebookRead/NotebookEdit
        // key it as `notebook_path`.
        crate::harness::plugin::MemArg::Path => (
            get("file_path")
                .or_else(|| get("notebook_path"))
                .unwrap_or("")
                .to_string(),
            None,
        ),
        crate::harness::plugin::MemArg::Pattern => (
            get("pattern")
                .or_else(|| get("path"))
                .unwrap_or("")
                .to_string(),
            None,
        ),
        crate::harness::plugin::MemArg::Command => (
            String::new(),
            get("command").map(|c| c.chars().take(200).collect::<String>()),
        ),
    };
    let recordable = match arg {
        crate::harness::plugin::MemArg::Command => detail.is_some(),
        _ => !path.is_empty(),
    };
    recordable.then_some((path, detail))
}

// ── V14 Phase C: token/cost usage tap ─────────────────────────────────────

/// Small ring of `tool_use_id -> tool name`, populated from every `tool_use`
/// block (ALL tools, unlike [`record_tool_events`]'s memory-kind filter —
/// usage accounting wants every tool named, not just the memory-worthy ones)
/// and consulted when the matching `tool_result` arrives so its estimated
/// chars can be attributed to a tool ("Read of `foo.rs` cost 18k twice" needs
/// the name). Bounded so a very long session can't grow it unboundedly —
/// oldest entries are evicted first, same ring posture as `mem_event`'s cap.
#[derive(Default)]
struct ToolNameRing {
    names: HashMap<String, String>,
    order: VecDeque<String>,
}

/// Ring cap — generous relative to how many tool calls a single session
/// realistically has outstanding at once (this only needs to bridge a
/// `tool_use` to its own `tool_result`, which normally arrives within the
/// same or next line).
const TOOL_NAME_RING_CAP: usize = 512;

impl ToolNameRing {
    fn insert(&mut self, id: String, name: String) {
        if !self.names.contains_key(&id) {
            self.order.push_back(id.clone());
            while self.order.len() > TOOL_NAME_RING_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.names.remove(&old);
                }
            }
        }
        self.names.insert(id, name);
    }

    fn get(&self, id: &str) -> Option<&str> {
        self.names.get(id).map(String::as_str)
    }

    /// Drop everything — called on session rotation (a new transcript file
    /// means old `tool_use` ids can never see a matching `tool_result`).
    fn clear(&mut self) {
        self.names.clear();
        self.order.clear();
    }
}

/// Pure: the DECLARED lane to attribute one transcript line's turn to.
/// `base_origin` is the caller's default ([`ORIGIN_SESSION`] for the parent
/// drain, [`ORIGIN_AGENT`] for a sub-agent file drain); an inline
/// `isSidechain:true` line — the 1.x sub-agent contract carried in the parent
/// transcript — is upgraded to the sidechain lane regardless.
///
/// V40 Phase G: these are the ids this harness DECLARES
/// ([`super::usage::TURN_SHAPE`]), spelled once in `usage.rs`, rather than a
/// core enum's two variants.
fn usage_origin(obj: &Value, base_origin: &'static str) -> &'static str {
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        ORIGIN_AGENT
    } else {
        base_origin
    }
}

/// Pure: extract a [`crate::graph::UsageEvent::Turn`] from an assistant
/// transcript line's `message.usage` block, tagged with `origin` (from
/// [`usage_origin`]). Tolerant of absent fields (older transcript lines, or a
/// partial line mid-stream before the block firms up) — missing token counts
/// default to 0, which is exactly right for the UPSERT-by-`msg_id` semantics in
/// `record_usage_event`: a later line carrying the SAME `msg_id` with the real
/// numbers overwrites this one in place rather than leaving a duplicate zero
/// row. `None` for any non-assistant line or an assistant line with no
/// `message.id`.
///
/// ## Historical-data caveat: usage recorded before Claude Code 2.1.214
///
/// Up to and including 2.1.213, Claude Code double-counted tokens and cost for
/// streamed responses (fixed in 2.1.214). Whatever the transcript's `usage`
/// block said is what we stored, so `usage_stat` rows written by sessions on a
/// pre-2.1.214 CLI can be inflated — treat old Usage-tab totals and any
/// cross-period cost comparison spanning that upgrade as approximate. Live data
/// is unaffected, and there is deliberately **no** correction/backfill logic:
/// we cannot tell an inflated row from a genuinely large turn after the fact,
/// and the CLI version that wrote a session is only known globally (the
/// `harness_versions` tripwire fed by [`note_cli_version`]), not per row.
/// `pub(crate)` for the V35 canary suite (harness/canary.rs).
pub(crate) fn parse_usage_line(
    obj: &Value,
    origin: &str,
) -> Option<crate::graph::UsageEvent> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = obj.get("message")?;
    let msg_id = message.get("id").and_then(Value::as_str)?.to_string();
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = message.get("usage");
    let tok = |k: &str| -> u32 {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };
    Some(crate::graph::UsageEvent::Turn {
        msg_id,
        model,
        in_tok: tok("input_tokens"),
        out_tok: tok("output_tokens"),
        cache_read: tok("cache_read_input_tokens"),
        cache_make: tok("cache_creation_input_tokens"),
        origin: origin.to_string(),
    })
}

/// Pure: `(tool_use_id, chars)` for every `tool_result` content block in a
/// user-role transcript line (the carrier for one or more parallel tool
/// results). `chars` is an estimated-token proxy for the result's size — no
/// exact token count exists for tool output, only for assistant messages.
/// `pub(crate)` for the V35 canary suite (harness/canary.rs).
pub(crate) fn extract_tool_results(obj: &Value) -> Vec<(String, usize)> {
    if obj.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    let Some(parts) = message_parts(obj) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = part.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let chars = tool_result_chars(part.get("content").unwrap_or(&Value::Null));
        out.push((id.to_string(), chars));
    }
    out
}

/// The text pieces of a `tool_result` block's `content`, which is either a
/// plain string or an array of blocks (`{"type":"text","text":...}` plus
/// possibly non-text blocks, e.g. images — only text blocks count). The one
/// shape-aware extraction both [`tool_result_chars`] and
/// [`tool_result_text`] build on, so the two readings of the same data can
/// never disagree.
fn tool_result_text_blocks(content: &Value) -> Vec<&str> {
    match content {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// Character length of a `tool_result` block's `content` — the estimated-
/// token proxy for usage accounting.
///
/// `pub(crate)` since V35 Phase L: the `PostToolUse` push path
/// ([`crate::harness::claude::hook::tool_result_chars`]) sizes the SAME shape
/// arriving over a hook payload instead of out of the transcript. Two readings
/// of one shape is exactly the drift this milestone exists to prevent, so the
/// push reuses this function rather than restating it — and the L1 canary that
/// proves the shape still parses therefore covers both paths at once.
pub(crate) fn tool_result_chars(content: &Value) -> usize {
    tool_result_text_blocks(content)
        .iter()
        .map(|t| t.chars().count())
        .sum()
}

// ── Session→commit provenance tap ─────────────────────────────────────────

/// The text of a `tool_result` block's `content`, joined with newlines —
/// built on the same [`tool_result_text_blocks`] extraction as
/// [`tool_result_chars`], for [`parse_commit_hashes`] to scan.
fn tool_result_text(content: &Value) -> String {
    tool_result_text_blocks(content).join("\n")
}

/// True when `content` marks its `tool_result` as an error (the API's
/// `is_error` flag) — a failed command's output must never be mined for
/// commit hashes (hook noise from an ABORTED commit could match the shape).
/// `pub(crate)` for the V35 canary suite (harness/canary.rs).
pub(crate) fn tool_result_is_error(part: &Value) -> bool {
    part.get("is_error").and_then(Value::as_bool) == Some(true)
}

/// Git global flags that take their value as a SEPARATE token (the `=`
/// forms are single tokens and skipped by the leading-`-` rule).
const GIT_VALUE_FLAGS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
];

/// Token-level check for "this shell command actually invokes `git commit`":
/// finds a `git` token (bare, path-suffixed, or `git.exe`), skips global
/// flags (and the separate value of [`GIT_VALUE_FLAGS`]), and requires the
/// first remaining token to be exactly `commit`. Chained commands work
/// because a later `git` token restarts the scan (`git add . && git commit`).
/// Unlike a substring check this does NOT match `git log --grep=commit` or
/// a mention of "commit" in a message argument.
fn is_git_commit_invocation(cmd: &str) -> bool {
    let mut tokens = cmd.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        let base = tok.trim_matches('"').trim_matches('\'');
        let is_git = base == "git"
            || base.ends_with("/git")
            || base.ends_with("\\git")
            || base.ends_with("git.exe");
        if !is_git {
            continue;
        }
        while let Some(next) = tokens.peek().copied() {
            if GIT_VALUE_FLAGS.contains(&next) {
                tokens.next();
                tokens.next(); // the flag's value
            } else if next.starts_with('-') {
                tokens.next();
            } else {
                break;
            }
        }
        if tokens.peek().copied() == Some("commit") {
            return true;
        }
        // Not a commit subcommand — keep scanning for a later `git` token.
    }
    false
}

/// Bounded insertion-ordered id set for commit tool_use ids awaiting their
/// result — membership is all that matters (unlike [`ToolNameRing`], which
/// maps to a value). Same eviction posture and cap as the name ring.
#[derive(Default)]
struct IdRing {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl IdRing {
    fn insert(&mut self, id: String) {
        if self.ids.insert(id.clone()) {
            self.order.push_back(id);
            while self.order.len() > TOOL_NAME_RING_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// Drop one resolved id. Its `order` entry is left behind and ages out
    /// with the ring — only `ids` membership matters.
    fn remove(&mut self, id: &str) {
        self.ids.remove(id);
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.order.clear();
    }
}

/// Extract created-commit hashes from a `git commit` invocation's output.
/// Git prints one summary line per commit created:
///
/// ```text
/// [develop 337bc57] feat(code-intel): session dates
/// [main (root-commit) abc1234] initial
/// [detached HEAD 1a2b3c4] fixup
/// ```
///
/// Scan: a line whose first char (after trim) is `[` with a closing `]`,
/// whose bracketed content's LAST whitespace-separated token is 7–40 hex
/// chars — that token is the (usually short) hash. Line-oriented and
/// dependency-free (no regex crate), tolerant of hook noise around the
/// summary line. Deduped, in output order. A false positive (bracketed log
/// noise ending in a hex-looking token) is harmless: recorded hashes are
/// prefix-matched against the REAL `git log` at query time, so a bogus one
/// simply never matches anything.
fn parse_commit_hashes(output: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in output.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some(close) = rest.find(']') else {
            continue;
        };
        let Some(tok) = rest[..close].split_whitespace().last() else {
            continue;
        };
        let is_hash = (7..=40).contains(&tok.len()) && tok.bytes().all(|b| b.is_ascii_hexdigit());
        if is_hash && !out.iter().any(|h| h == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

/// Attach commits to their session as they happen: a `tool_use` whose shell
/// `command` is a real `git commit` invocation ([`is_git_commit_invocation`])
/// marks its id in `commit_calls`; the paired SUCCESSFUL `tool_result`'s
/// text is scanned for git's `[branch hash]` summary and every hash found is
/// recorded against the session. Error results are skipped entirely — an
/// aborted commit's hook noise must never be mined for hashes. A successful
/// commit that printed no summary (`git commit -q`) still gets provenance
/// via a `git rev-parse HEAD` fallback ([`spawn_head_fallback`]). Sidechain
/// (sub-agent) lines are NOT skipped — a sub-agent's commit is still this
/// session's commit. A no-op when memory isn't wired.
///
/// OpenCode has no equivalent tap: its `chat.message` plugin ingress (see
/// `offload::loopback::handle_memory_event`) doesn't carry tool outputs, so
/// OpenCode sessions fall back to the Workbench's time-window association.
fn record_commit_events(
    obj: &Value,
    commit_calls: &mut IdRing,
    project_dir: &Path,
    session_id: &str,
    ctx: &OobContext,
) {
    if ctx.mem.is_none() {
        return;
    }
    let Some(parts) = message_parts(obj) else {
        return;
    };
    match obj.get("type").and_then(Value::as_str) {
        // Mark candidate commit commands (assistant lines). `--dry-run`
        // never creates a commit and prints no summary, so tracking it
        // would only arm the HEAD fallback with a false positive — skip.
        Some("assistant") => {
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let Some(id) = part.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(cmd) = part
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if is_git_commit_invocation(cmd) && !cmd.contains("--dry-run") {
                    commit_calls.insert(id.to_string());
                }
            }
        }
        // Resolve results (user lines) for marked ids.
        Some("user") => {
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(id) = part.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                if !commit_calls.contains(id) {
                    continue;
                }
                commit_calls.remove(id);
                if tool_result_is_error(part) {
                    continue; // the commit failed — nothing was created.
                }
                let text = tool_result_text(part.get("content").unwrap_or(&Value::Null));
                let hashes = parse_commit_hashes(&text);
                if hashes.is_empty() {
                    // Succeeded but printed no summary (`git commit -q`, or
                    // output swallowed by a wrapper) — resolve HEAD instead.
                    spawn_head_fallback(ctx, project_dir, session_id);
                    continue;
                }
                for hash in hashes {
                    debug!(tab = ?ctx.tab, %hash, "Claude OOB: session commit caught");
                    ctx.record_commit(project_dir, session_id, &hash);
                }
            }
        }
        _ => {}
    }
}

/// Quiet-commit fallback: a commit-shaped command succeeded but its output
/// carried no `[branch hash]` summary line — resolve the repo's HEAD right
/// now and record that. The transcript is tailed near-real-time (200ms
/// poll), so HEAD is still the commit the command just created except in
/// pathological rapid-fire cases; recording HEAD is then still a commit this
/// session made moments ago. Best-effort: any git failure is dropped
/// silently (the time-window fallback still covers the commit).
fn spawn_head_fallback(ctx: &OobContext, project_dir: &Path, session_id: &str) {
    let Some(mem) = ctx.mem.clone() else { return };
    let root = project_dir.to_path_buf();
    let session_id = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        let git_ctx = crate::workbench::git::GitCtx::discover(&root);
        match crate::workbench::git::run(&git_ctx, &["rev-parse", "HEAD"], None).await {
            Ok(out) if out.success() => {
                let hash = out.stdout.trim().to_string();
                if !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    debug!(%hash, "Claude OOB: session commit resolved via HEAD fallback");
                    mem.record_session_commit(&root, &session_id, &hash);
                }
            }
            _ => {}
        }
    });
}

/// V14 Phase C: feed the token/cost X-ray from one transcript line — a `Turn`
/// event from an assistant message's `usage` block, and a `ToolResult` event
/// per `tool_result` block in a tool-carrier user line (joined to its tool
/// name via `tool_names`). A no-op when memory isn't wired (mirrors
/// `record_tool_events`). Unlike `record_tool_events`, sidechain (sub-agent)
/// lines are NOT skipped: a sub-agent's tokens are real spend against the
/// same session and must be counted, even though its file touches aren't
/// tracked as the parent's working set. Under the 1.x contract those lines
/// arrive inline through the parent drain; under the 2.x contract the same
/// lines live in `subagents/*.jsonl` and reach this function via
/// [`SubagentState::scan`] instead.
/// `base_origin` is the caller's default attribution for the turn: the parent
/// drain passes [`ORIGIN_SESSION`], [`SubagentState::scan`] passes
/// [`ORIGIN_AGENT`]. Either way an inline `isSidechain:true` line (the
/// 1.x sub-agent contract, which arrives through the parent drain) is forced to
/// `Agent` — a sub-agent's tokens are the parent session's spend, tagged as
/// agent so the S/A chart can split them out. Tool-result rows carry no origin.
fn record_usage(
    obj: &Value,
    tool_names: &mut ToolNameRing,
    project_dir: &Path,
    session_id: &str,
    ctx: &OobContext,
    base_origin: &'static str,
) {
    if ctx.mem.is_none() {
        return;
    }

    // Learn tool_use_id -> name for every tool_use block, regardless of
    // whether the native table gives it a memory kind.
    if let Some(parts) = message_parts(obj) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let (Some(id), Some(name)) = (
                    part.get("id").and_then(Value::as_str),
                    part.get("name").and_then(Value::as_str),
                ) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    if let Some(event) = parse_usage_line(obj, usage_origin(obj, base_origin)) {
        ctx.record_usage(project_dir, session_id, "claude", event);
    }

    // V35 Phase L — arbitration, on THIS tap only. The `UsageEvent::Turn` above
    // is NOT arbitrated and never will be: no hook payload carries token
    // counts, so `claude.transcript.usage` stays Tier C here
    // permanently-until-upstream-changes. Only the tool-result SIZING moved,
    // and only for a tab whose all-tools `PostToolUse` entry declares it —
    // otherwise one result would be counted twice, in the same `msg_id`-less
    // row shape, with nothing downstream able to tell the copies apart.
    //
    // The `tool_names` ring above is fed regardless, because it is what joins a
    // `tool_use` id to a name for the OTHER readers (and for this one again if
    // a relaunch retires the hello).
    if ctx.pushed("claude", crate::harness::chp::EV_SESSION_TOOL_RESULT) {
        return;
    }
    for (tool_use_id, chars) in extract_tool_results(obj) {
        let tool = tool_names.get(&tool_use_id).map(str::to_string);
        ctx.record_usage(
            project_dir,
            session_id,
            "claude",
            crate::graph::UsageEvent::ToolResult {
                tool,
                chars: chars as u32,
            },
        );
    }
}

// ── Sub-agent transcript tail (2.x contract) + drift canary ───────────────

/// Where the 2.x CLIs write sub-agent transcripts: one JSONL per agent at
/// `<projects-root>/<session_id>/subagents/agent-<id>.jsonl` (plus a small
/// `agent-<id>.meta.json` we don't read). The lines inside carry the same
/// shape as the parent transcript (including `isSidechain:true`).
fn subagents_dir(root: &Path, session_id: &str) -> PathBuf {
    root.join(session_id).join("subagents")
}

/// Ticks a drift condition must hold before it's reported: the parent file
/// and the subagents dir are written by separate handles, so one 200ms poll
/// can catch a launch line before its transcript file exists (or vice
/// versa). Three ticks (~600ms) outlives any such write-order race.
const DRIFT_CONFIRM_TICKS: u32 = 3;

/// Tail state for one sub-agent transcript file: read offset plus the same
/// tool-name / commit-call rings the parent tail keeps (tool_use ids never
/// cross files, so per-file rings can't mis-join).
#[derive(Default)]
struct SubagentFile {
    offset: u64,
    tool_names: ToolNameRing,
    commit_calls: IdRing,
}

/// Session-scoped sub-agent state: the per-file tails for
/// [`subagents_dir`]'s `*.jsonl`, plus the facts the drift canary reasons
/// over. Reset on session rotation (a new transcript file is a new session,
/// with its own subagents dir).
#[derive(Default)]
struct SubagentState {
    files: HashMap<PathBuf, SubagentFile>,
    /// Files already present on the FIRST scan seek to EOF when the app
    /// attached mid-session — mirrors the parent tail's backlog skip.
    skip_backlog: bool,
    scanned_once: bool,
    /// Canary facts (see [`Self::drift_tick`]).
    launch_seen: bool,
    completion_seen: bool,
    sidechain_seen: bool,
    drift_ticks: u32,
    drift_reported: bool,
}

impl SubagentState {
    /// Fresh state for a new session file. `skip_backlog` is the parent
    /// tail's first-attach flag at rotation time.
    fn reset(&mut self, skip_backlog: bool) {
        *self = SubagentState {
            skip_backlog,
            ..SubagentState::default()
        };
    }

    /// One poll tick: discover and drain `<root>/<sid>/subagents/*.jsonl`,
    /// feeding ONLY the usage and commit-provenance taps — a sub-agent's
    /// tokens and commits are real spend/output of the parent session, but
    /// its file touches, prompts, and text stay out of the parent's working
    /// set, turn clocks, avatar state, and TTS (the same split the inline
    /// sidechain contract had). A no-op when memory isn't wired: every tap
    /// this feeds is mem-gated, so the extra IO would buy nothing.
    fn scan(&mut self, root: &Path, session_id: &str, project_dir: &Path, ctx: &OobContext) {
        if ctx.mem.is_none() {
            return;
        }
        let dir = subagents_dir(root, session_id);
        let first_scan = !self.scanned_once;
        self.scanned_once = true;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return, // no sub-agents (yet) — the common case.
        };
        let seek_to_eof = first_scan && self.skip_backlog;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let tail = self
                .files
                .entry(path.clone())
                .or_insert_with(|| SubagentFile {
                    offset: if seek_to_eof { len } else { 0 },
                    ..SubagentFile::default()
                });
            if len <= tail.offset {
                continue; // nothing new — skip the open entirely.
            }
            let Some((complete, new_offset)) = read_complete_lines(&path, tail.offset) else {
                continue;
            };
            tail.offset = new_offset;
            for line in complete.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(obj) = parse_transcript_line(&path, line) {
                    // Sub-agent transcript file (2.x contract): every turn here
                    // is Agent spend against the parent session.
                    record_usage(
                        &obj,
                        &mut tail.tool_names,
                        project_dir,
                        session_id,
                        ctx,
                        ORIGIN_AGENT,
                    );
                    record_commit_events(
                        &obj,
                        &mut tail.commit_calls,
                        project_dir,
                        session_id,
                        ctx,
                    );
                }
            }
        }
    }

    /// The sub-agent drift canary: report (once per session, as an Activity
    /// event with `tool: "subagent_drift"` that the advisor's
    /// `drift.subagent_transcripts.v1` rule reads) when the transcript
    /// contract has visibly moved again. Two detectable shapes:
    ///
    ///  • an agent genuinely completed but its traffic showed up neither
    ///    inline (`isSidechain` lines) nor as `subagents/*.jsonl` — the
    ///    transcripts moved somewhere this tail doesn't watch, and the
    ///    session's sub-agent token spend is being silently dropped;
    ///  • `subagents/*.jsonl` exist but no launch `tool_use` was ever
    ///    recognized — the launcher tool was renamed again (as Task→Agent
    ///    was); usage still counts, but the agents-active avatar hold is
    ///    dead. Suppressed when we attached mid-session (`skip_backlog`):
    ///    the launches may simply predate the attach.
    ///
    /// A simultaneous rename AND relocation is invisible from this vantage —
    /// nothing observable remains — so this canary covers single-axis drift
    /// only. Conditions must hold [`DRIFT_CONFIRM_TICKS`] consecutive ticks
    /// (write-order races between the two locations resolve within one).
    /// Mem-gated like `scan`: without memory `files` never populates, which
    /// would make the "transcripts missing" arm a false constant.
    fn drift_tick(&mut self, project_dir: &Path, session_id: &str, ctx: &OobContext) {
        if ctx.mem.is_none() || self.drift_reported {
            return;
        }
        let Some(summary) = self.drift_condition() else {
            self.drift_ticks = 0;
            return;
        };
        self.drift_ticks += 1;
        if self.drift_ticks < DRIFT_CONFIRM_TICKS {
            return;
        }
        self.drift_reported = true;
        debug!(
            session = session_id,
            summary, "Claude OOB: sub-agent contract drift"
        );
        report_subagent_drift(project_dir, session_id, summary);
    }

    /// Pure half of [`Self::drift_tick`]: which drift condition currently
    /// holds, if any. Split out so the state machine is testable without
    /// touching the global Activity store.
    fn drift_condition(&self) -> Option<&'static str> {
        if self.completion_seen && !self.sidechain_seen && self.files.is_empty() {
            return Some(
                "sub-agent completed but no sidechain lines and no subagents/*.jsonl — \
                 transcripts moved; sub-agent token spend is not being counted",
            );
        }
        if !self.launch_seen && !self.skip_backlog && !self.files.is_empty() {
            return Some(
                "subagents/*.jsonl present but no Task/Agent launch tool_use recognized — \
                 launcher tool renamed; agents-active tracking is blind",
            );
        }
        None
    }
}

/// Record one sub-agent drift report in the Activity store (`source:
/// "harness"`, `tool: "subagent_drift"` — same channel discipline as the
/// hook shims' `contract_drift` events, which the advisor also reads from
/// the ring). Fire-and-forget; the caller rate-limits to once per session.
fn report_subagent_drift(project_dir: &Path, session_id: &str, summary: &str) {
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            project_dir.to_string_lossy().to_string(),
            "harness".to_string(),
            "subagent_drift".to_string(),
            summary.to_string(),
            summary.chars().count(),
            0,
            false, // a drift report is never "ok" — it flags the entry in the feed
            // Same shape as the shim's `contract_drift`: about the harness
            // rather than a tab, but the session is known.
            crate::activity::Attribution::Unattributed,
            Some(session_id.to_string()),
            None,
            None,
        ),
        request: format!("sub-agent transcript contract drift (session {session_id})"),
        response: summary.to_string(),
    });
}

/// Parse one transcript JSONL line, or `None` when it isn't valid JSON.
///
/// Claude Code documents the transcript format as unstable ("can break on any
/// release"), so a line this tap cannot read must never stop the tail: it is
/// skipped with a log naming the file, and the NEXT line is drained normally.
/// Both drains ([`drain_new_lines`] and [`SubagentState::scan`]) go through here
/// so the skip-and-log posture holds for parent and sub-agent transcripts alike.
/// Partial trailing lines never reach this function — [`read_complete_lines`]
/// holds them back until their newline lands — so a failure here is a genuinely
/// malformed line, not a mid-write read.
///
/// **Review M10 — the skip contract needs a consumer at the shipped log level.**
/// The default level is Info (`settings/schema.rs`), so a `debug!` made the one
/// failure mode this contract exists to detect — a format change that fails
/// EVERY line — completely silent. The FIRST skip per transcript file now
/// `warn!`s; later skips carry the running count at debug, so a single malformed
/// line in a healthy tail can't warn-spam.
fn parse_transcript_line(path: &Path, line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => Some(v),
        Err(e) => {
            let prefix: String = line.chars().take(120).collect();
            let count = note_skipped_line(path);
            if count == 1 {
                warn!(
                    file = %path.display(),
                    error = %e,
                    line = %prefix,
                    "Claude OOB: unparseable transcript line skipped — transcript format may have \
                     drifted (further skips in this file log at debug)"
                );
            } else {
                debug!(
                    file = %path.display(),
                    error = %e,
                    line = %prefix,
                    skipped = count,
                    "Claude OOB: unparseable transcript line skipped"
                );
            }
            None
        }
    }
}

/// How many lines this process has failed to parse in `path`, including this
/// one. Process-global (not per-tap) so two taps on the same project can't warn
/// twice for the same file; the map only ever holds files that actually failed.
fn note_skipped_line(path: &Path) -> u64 {
    static SKIPS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, u64>>> =
        std::sync::OnceLock::new();
    let map = SKIPS.get_or_init(Default::default);
    let mut map = map.lock().unwrap_or_else(|p| p.into_inner());
    bump_skip(&mut map, path)
}

/// Pure half of [`note_skipped_line`] — the throttle's whole decision is "is
/// this the first failure for this file?", so it is pinned without the static.
fn bump_skip(map: &mut HashMap<PathBuf, u64>, path: &Path) -> u64 {
    let entry = map.entry(path.to_path_buf()).or_insert(0);
    *entry += 1;
    *entry
}

/// Read complete new lines from `path` starting at `offset`: the chunk up to
/// (and including) the last newline, plus the advanced offset. `None` when
/// nothing new/complete is readable — a trailing partial line waits for the
/// next tick (offset not advanced past it), and a vanished/rotated file just
/// retries next tick.
fn read_complete_lines(path: &Path, offset: u64) -> Option<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(offset);
    if len <= offset {
        return None; // nothing new (or truncated/rotated).
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    let last_nl = buf.rfind('\n')?;
    buf.truncate(last_nl + 1);
    let new_offset = offset + buf.len() as u64;
    Some((buf, new_offset))
}

/// `~/.claude/projects/` — the root every per-project transcript directory
/// hangs off. `None` if no home dir.
///
/// Split out of [`project_root`] for the V35 Phase D live probe, which has no
/// project scope: it looks for the newest transcript ANYWHERE under this root
/// (`harness/probe.rs`). One definition of where Claude Code keeps transcripts,
/// so the probe cannot verify a layout the tap does not actually read.
pub(crate) fn projects_root() -> Option<PathBuf> {
    Some(home_dir()?.join(".claude").join("projects"))
}

/// `~/.claude/projects/<slug>/` for `project_dir`. `None` if no home dir.
/// `pub(crate)` for the same probe, which prefers the transcript of the
/// directory it was run in before falling back to the newest anywhere.
pub(crate) fn project_root(project_dir: &Path) -> Option<PathBuf> {
    Some(projects_root()?.join(slug_for(project_dir)))
}

/// Claude Code's project-dir slug: every path separator and `:` becomes `-`.
/// e.g. `P:\Documents\foo` -> `P--Documents-foo`.
fn slug_for(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| {
            if c == '\\' || c == '/' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// What a tap holding a `--session-id` pin should do this tick. Pure so the
/// decision is unit-testable without a filesystem or a clock.
///
/// **Passing `--session-id` is a request, not a guarantee** (2026-08-09, found
/// in the field). Observed on three live tabs: each carried a DISTINCT
/// `--session-id` UUID on its argv and none carried a `--resume`/`--continue`,
/// yet two of them were actively writing transcripts under different,
/// pre-existing session ids — only the tab starting a fresh conversation had a
/// `*.jsonl` matching its pin.
///
/// The mechanism is not established (restoring a prior conversation overriding
/// the flag is the obvious candidate, but it was not proven); what IS
/// established is that the flag's effect must be verified rather than assumed.
///
/// So the pin is treated as a CLAIM TO BE VERIFIED, and the transcript's
/// existence is the verification. Everything downstream — the live-session
/// registry entry, the registry's `pinned` flag, and therefore the ambiguity
/// exemption — waits on that, because a session id we merely asked for names a
/// conversation that may not exist.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PinStep {
    /// The pinned transcript exists ⇒ the harness honoured it. Follow that file
    /// and nothing else, and claim the identity.
    Follow,
    /// No pinned transcript. Behave exactly as an unpinned tap does —
    /// newest-wins, no identity claim, ambiguity rules re-applied — while
    /// CONTINUING to watch for the file, so a conversation that starts writing
    /// later still upgrades to `Follow`.
    ///
    /// Deliberately not a blocking wait. An earlier design parked the tap until
    /// the file appeared, which cost every restored tab its TTS, usage and
    /// memory for the whole window — a regression against not pinning at all.
    /// Falling back immediately is never worse than pre-V34, and costs only the
    /// isolation a restored tab was never going to get.
    Fallback,
}

pub(crate) fn pin_step(pinned_present: bool) -> PinStep {
    if pinned_present {
        PinStep::Follow
    } else {
        PinStep::Fallback
    }
}

/// Newest `*.jsonl` (by mtime) under `root`, or `None` if the dir is missing
/// or empty. (The doc comment had drifted onto [`PinStep`] above; restored
/// here in V35 Phase D.)
///
/// `pub(crate)` for the V35 Phase D live probe — same "newest wins" rule the
/// tap itself uses, so the probe reads the file the tap would have read.
pub(crate) fn newest_jsonl(root: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, p)| p)
}

/// Resolve the user's home directory without pulling in a new dependency.
///
/// `pub(crate)` since V35 Phase H: [`crate::harness::capture`] needs the same
/// answer for its data-dir fallback, and a second three-line copy of the
/// `USERPROFILE`-then-`HOME` order is exactly the kind of duplicate that ends
/// up disagreeing on one platform.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── H1-R2: the tap heartbeat's decision logic ──
    //
    // The spawned task itself isn't unit-testable here (marking needs a live
    // `GraphService`, which needs a Tauri `AppHandle`), so the retire/tick
    // ordering decision is factored into `TapHeartbeat` and tested directly;
    // the registry-side property it relies on ("refresh restores a TTL-stale
    // claim") is pinned in `graph::service`'s
    // `refreshing_a_tab_root_restores_a_ttl_stale_claim`.

    fn hb_ctx() -> OobContext {
        let (tts_tx, _tts_rx) = tokio::sync::mpsc::channel(4);
        let (sig_tx, _sig_rx) = tokio::sync::mpsc::channel(4);
        let defaults = crate::settings::Settings::default();
        OobContext {
            tab: crate::state::TabId::from_str("claude"),
            tts: tts_tx,
            state_signals: sig_tx,
            settings: crate::settings::SettingsHandle::new(
                defaults.clone(),
                defaults,
                std::env::temp_dir(),
            ),
            cancel: tokio_util::sync::CancellationToken::new(),
            mem: None,
            pushes: None,
        }
    }

    #[test]
    fn heartbeat_ticks_until_it_is_retired() {
        let ctx = hb_ctx();
        let root = Path::new("/home/u/.claude/projects/P--proj");
        let hb = TapHeartbeat::default();
        assert!(hb.tick(&ctx, root), "a live heartbeat refreshes the claim");
        hb.note_session("ses_a");
        assert!(hb.tick(&ctx, root));
        // The guard's `Drop` retires BEFORE it clears the registry; from that
        // moment no tick may write, or a closed tab's claim would be
        // resurrected and keep suppressing the survivor's scoping for a TTL.
        hb.retire();
        assert!(!hb.tick(&ctx, root), "a retired heartbeat must not re-mark");
        assert!(!hb.tick(&ctx, root), "and stays retired");
    }

    #[test]
    fn heartbeat_only_reports_a_confirmed_session() {
        // Until the drain loop confirms one, the heartbeat refreshes the ROOT
        // claim only — it must never invent a `live_sessions` entry for the
        // stale transcript a fresh tap first attaches to.
        let hb = TapHeartbeat::default();
        assert!(hb.state.lock().unwrap().session.is_none());
        hb.note_session("ses_a");
        assert_eq!(hb.state.lock().unwrap().session.as_deref(), Some("ses_a"));
        // Session rotation (`/clear`, a new file) is carried through.
        hb.note_session("ses_b");
        assert_eq!(hb.state.lock().unwrap().session.as_deref(), Some("ses_b"));
    }

    #[test]
    fn the_skip_log_warns_once_per_file_then_counts() {
        // Review M10: the drift signal must be visible at the shipped Info
        // level, but one bad line in a healthy tail must not warn-spam. The
        // throttle's whole decision is "first failure for this file?".
        let mut seen = HashMap::new();
        let a = Path::new("a.jsonl");
        let b = Path::new("b.jsonl");
        assert_eq!(bump_skip(&mut seen, a), 1, "first skip in a warns");
        assert_eq!(bump_skip(&mut seen, a), 2, "later skips count at debug");
        assert_eq!(bump_skip(&mut seen, a), 3);
        assert_eq!(bump_skip(&mut seen, b), 1, "a different file warns once too");
    }

    // ── V34: the `--session-id` pin ──────────────────────────────────────

    #[test]
    fn the_pinned_transcript_existing_is_what_verifies_the_pin() {
        assert_eq!(pin_step(true), PinStep::Follow);
    }

    #[test]
    fn an_absent_pinned_transcript_runs_unpinned_rather_than_stalling() {
        // Found in the field (2026-08-09): a tab can run under a session id
        // that is NOT the one cImp pinned — three live tabs each carried a
        // distinct `--session-id`, and two were writing transcripts under
        // different, pre-existing ids. So the pinned file may never appear.
        //
        // The tap must therefore degrade to exactly the pre-V34 behaviour
        // immediately. An earlier design parked the tap waiting for the file,
        // which cost every restored tab its TTS, usage and memory for two
        // minutes — strictly worse than never pinning.
        assert_eq!(pin_step(false), PinStep::Fallback);
    }

    #[test]
    fn an_unparseable_line_is_skipped_not_fatal() {
        // The parse boundary itself: a malformed line yields None and the tail
        // carries on with the next one.
        let path = Path::new("drifted.jsonl");
        assert!(parse_transcript_line(path, "{not json").is_none());
        assert!(parse_transcript_line(path, r#"{"type":"user"}"#).is_some());
    }

    #[test]
    fn slug_replaces_separators_and_colon() {
        let s = slug_for(Path::new(r"P:\Documents\AI-private\cc-avatar\cctts"));
        assert_eq!(s, "P--Documents-AI-private-cc-avatar-cctts");
    }

    #[test]
    fn assistant_texts_skips_thinking_and_tools() {
        let obj: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"Hello there."},
                {"type":"tool_use","name":"Bash"}
            ]}}"#,
        )
        .unwrap();
        let got = assistant_texts(&obj);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "Hello there.");
        assert!(got[0].0.starts_with("m1:"));
    }

    #[test]
    fn non_assistant_lines_yield_nothing() {
        let obj: Value =
            serde_json::from_str(r#"{"type":"user","message":{"content":"hi"}}"#).unwrap();
        assert!(assistant_texts(&obj).is_empty());
    }

    #[test]
    fn empty_text_blocks_are_ignored() {
        let obj: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"   "}]}}"#,
        )
        .unwrap();
        assert!(assistant_texts(&obj).is_empty());
    }

    // --- Session→commit provenance (record_commit_events) ---

    #[test]
    fn parse_commit_hashes_reads_git_summary_lines() {
        let out =
            "[develop 337bc57] feat(code-intel): session dates\n 5 files changed, 60 insertions(+)";
        assert_eq!(parse_commit_hashes(out), vec!["337bc57"]);
        // Root-commit and detached-HEAD decorations still end with the hash.
        assert_eq!(
            parse_commit_hashes("[main (root-commit) abc1234] initial"),
            vec!["abc1234"]
        );
        assert_eq!(
            parse_commit_hashes("[detached HEAD 1a2b3c4] fixup"),
            vec!["1a2b3c4"]
        );
        // Two commits from one chained command; duplicates collapse.
        let two = "[develop aaa1111] one\nnoise\n[develop bbb2222] two\n[develop bbb2222] two";
        assert_eq!(parse_commit_hashes(two), vec!["aaa1111", "bbb2222"]);
        // An all-digit token is still a legitimate short hash (~4% of them
        // are); bogus ones are filtered at query time by prefix-matching
        // against the real log.
        assert_eq!(
            parse_commit_hashes("[develop 1234567] all-digit hash"),
            vec!["1234567"]
        );
        // Non-hex or short tokens are not hashes.
        assert!(parse_commit_hashes("[branch xyzzy99] not hex").is_empty());
        assert!(parse_commit_hashes("[short ab12] too short").is_empty());
        assert!(parse_commit_hashes("no brackets at all").is_empty());
    }

    #[test]
    fn is_git_commit_invocation_matches_real_commits_only() {
        assert!(is_git_commit_invocation("git commit -m 'x'"));
        assert!(is_git_commit_invocation("git -C sub commit --amend"));
        assert!(is_git_commit_invocation("git -c user.name=x commit"));
        assert!(is_git_commit_invocation("git add . && git commit -m 'y'"));
        assert!(is_git_commit_invocation(
            r#"& "C:\Program Files\Git\bin\git.exe" commit -m z"#
        ));
        assert!(!is_git_commit_invocation("git status"));
        assert!(!is_git_commit_invocation("git log --grep=commit"));
        assert!(!is_git_commit_invocation("git log --grep commit"));
        assert!(!is_git_commit_invocation("echo commit && git status"));
        assert!(!is_git_commit_invocation("cargo build"));
    }

    #[test]
    fn record_commit_events_is_a_noop_without_graph_memory() {
        // ctx.mem is None; must not panic and must not mark the ring (the
        // early return happens before the tool_use scan).
        let (ctx, _rx) = agent_ctx();
        let mut ring = IdRing::default();
        let line: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git commit -m x"}}
            ]}}"#,
        )
        .unwrap();
        record_commit_events(&line, &mut ring, Path::new("."), "s1", &ctx);
        assert!(!ring.contains("toolu_1"));
    }

    #[test]
    fn tool_result_is_error_reads_the_flag() {
        let err: Value = serde_json::from_str(
            r#"{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"hook failed"}"#,
        )
        .unwrap();
        let ok: Value = serde_json::from_str(
            r#"{"type":"tool_result","tool_use_id":"t1","content":"[develop 337bc57] x"}"#,
        )
        .unwrap();
        assert!(tool_result_is_error(&err));
        assert!(!tool_result_is_error(&ok));
    }

    #[test]
    fn id_ring_membership_and_eviction() {
        let mut ring = IdRing::default();
        ring.insert("a".to_string());
        assert!(ring.contains("a"));
        ring.remove("a");
        assert!(!ring.contains("a"));
        for i in 0..(TOOL_NAME_RING_CAP + 1) {
            ring.insert(format!("id_{i}"));
        }
        assert!(!ring.contains("id_0")); // oldest evicted at cap
        assert!(ring.contains(&format!("id_{TOOL_NAME_RING_CAP}")));
    }

    // --- Task sub-agent tracking (update_agents) ---

    use crate::settings::{Settings, SettingsHandle};
    use crate::state::TabId;
    use crate::tts::TtsRequest;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn agent_ctx() -> (OobContext, mpsc::Receiver<StateSignal>) {
        let (tts_tx, _tts_rx) = mpsc::channel::<TtsRequest>(64);
        let (sig_tx, sig_rx) = mpsc::channel::<StateSignal>(64);
        let defaults = Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());
        let ctx = OobContext {
            tab: TabId::Claude,
            tts: tts_tx,
            state_signals: sig_tx,
            settings,
            cancel: CancellationToken::new(),
            mem: None,
            pushes: None,
        };
        (ctx, sig_rx)
    }

    fn obj(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Assistant message launching one sub-agent via the named tool.
    fn launch_named(id: &str, name: &str) -> Value {
        obj(&format!(
            r#"{{"type":"assistant","message":{{"id":"a1","content":[
                {{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}
            ]}}}}"#
        ))
    }

    /// Assistant message launching one Task agent (the 1.x tool name).
    fn launch(id: &str) -> Value {
        launch_named(id, "Task")
    }

    /// User message carrying the tool_result for `id`.
    fn result(id: &str) -> Value {
        obj(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"{id}","content":"done"}}
            ]}}}}"#
        ))
    }

    #[test]
    fn launch_then_result_emits_active_then_inactive() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();

        update_agents(&launch("toolu_a"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: true, .. })
        ));

        update_agents(&result("toolu_a"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: false, .. })
        ));
        assert!(sig.try_recv().is_err(), "no further edges");
        assert!(agents.is_empty());
    }

    #[test]
    fn parallel_launch_flips_active_once_and_inactive_on_last() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();

        // Two agents launched in one assistant message → single active edge.
        let both = obj(r#"{"type":"assistant","message":{"id":"a1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Task","input":{}},
                {"type":"tool_use","id":"toolu_2","name":"Task","input":{}}
            ]}}"#);
        update_agents(&both, &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: true, .. })
        ));
        assert!(sig.try_recv().is_err(), "only one active edge for a batch");

        // First result: still one outstanding → no edge.
        update_agents(&result("toolu_1"), &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "still one agent running");

        // Last result: crosses to zero → inactive edge.
        update_agents(&result("toolu_2"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: false, .. })
        ));
    }

    #[test]
    fn non_task_tool_use_is_ignored() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let bash = obj(r#"{"type":"assistant","message":{"id":"a1","content":[
                {"type":"tool_use","id":"toolu_b","name":"Bash","input":{}}
            ]}}"#);
        update_agents(&bash, &mut agents, &ctx);
        assert!(
            sig.try_recv().is_err(),
            "non-Task tool must not mark agents active"
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn sidechain_lines_do_not_perturb_the_count() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        // A sub-agent's own internal Task-shaped line, marked isSidechain.
        let side = obj(
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"s1","content":[
                {"type":"tool_use","id":"toolu_nested","name":"Task","input":{}}
            ]}}"#,
        );
        update_agents(&side, &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "sidechain must be ignored");
        assert!(agents.is_empty());
    }

    #[test]
    fn stray_result_for_untracked_id_is_noop() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        // Result for a tool we never tracked (e.g. a Read) must not emit.
        update_agents(&result("toolu_never_seen"), &mut agents, &ctx);
        assert!(sig.try_recv().is_err());
    }

    #[test]
    fn agent_named_launch_is_tracked_like_task() {
        // The 2.x CLIs renamed the launcher tool Task → Agent; both names
        // must hold the avatar (this pins the AGENT_TOOL_NAMES contract).
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();

        update_agents(&launch_named("toolu_a", "Agent"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: true, .. })
        ));

        update_agents(&result("toolu_a"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: false, .. })
        ));
        assert!(agents.is_empty());
    }

    #[test]
    fn update_agents_reports_launch_and_completion_deltas() {
        let (ctx, _sig) = agent_ctx();
        let mut agents = HashSet::new();

        let d = update_agents(&launch("toolu_a"), &mut agents, &ctx);
        assert!(d.launched && !d.completed);

        // A stray result for an untracked id is NOT a completion.
        let d = update_agents(&result("toolu_other"), &mut agents, &ctx);
        assert!(!d.launched && !d.completed);

        // The tracked id's result is.
        let d = update_agents(&result("toolu_a"), &mut agents, &ctx);
        assert!(!d.launched && d.completed);

        // A turn-boundary reclaim (user prompt clearing orphans) is not a
        // completion either — the canary must not trust orphaned agents to
        // have written transcripts.
        update_agents(&launch("toolu_b"), &mut agents, &ctx);
        let prompt = obj(r#"{"type":"user","message":{"content":"next question"}}"#);
        let d = update_agents(&prompt, &mut agents, &ctx);
        assert!(!d.launched && !d.completed);
        assert!(agents.is_empty());
    }

    #[test]
    fn user_prompt_clears_orphaned_agents() {
        // Esc-interrupt: a Task launched but its tool_result never arrives.
        // The next genuine user prompt is a turn boundary that reclaims it,
        // emitting the inactive edge so the avatar can settle.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        update_agents(&launch("toolu_orphan"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: true, .. })
        ));

        // Plain-string user prompt.
        let prompt =
            obj(r#"{"type":"user","message":{"role":"user","content":"try again please"}}"#);
        update_agents(&prompt, &mut agents, &ctx);
        assert!(
            agents.is_empty(),
            "turn boundary must clear orphaned agents"
        );
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: false, .. })
        ));
    }

    #[test]
    fn user_prompt_with_text_block_is_a_boundary() {
        // Some prompts arrive as a content array with a text block rather than
        // a bare string — still a turn boundary.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        update_agents(&launch("toolu_x"), &mut agents, &ctx);
        let _ = sig.try_recv();
        let prompt =
            obj(r#"{"type":"user","message":{"content":[{"type":"text","text":"next"}]}}"#);
        update_agents(&prompt, &mut agents, &ctx);
        assert!(agents.is_empty());
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: false, .. })
        ));
    }

    #[test]
    fn tool_result_carrier_is_not_a_boundary() {
        // A user message that carries only tool_results (the normal agent-
        // result path) must NOT be treated as a turn boundary — it should
        // remove just its own id, leaving other agents outstanding.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let both = obj(r#"{"type":"assistant","message":{"id":"a1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Task","input":{}},
                {"type":"tool_use","id":"toolu_2","name":"Task","input":{}}
            ]}}"#);
        update_agents(&both, &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: true, .. })
        ));

        // tool_result for one — is_user_prompt is false (only tool_result
        // parts), so it removes toolu_1 and leaves toolu_2 running: no edge.
        update_agents(&result("toolu_1"), &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "one agent still outstanding");
        assert_eq!(agents.len(), 1);
        assert!(agents.contains("toolu_2"));
    }

    #[test]
    fn user_prompt_with_no_agents_is_silent() {
        // A turn boundary with nothing outstanding must not emit a phantom edge.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let prompt = obj(r#"{"type":"user","message":{"content":"hello"}}"#);
        update_agents(&prompt, &mut agents, &ctx);
        assert!(sig.try_recv().is_err());
    }

    // ── V14 Phase C: usage tap (parse_usage_line / extract_tool_results) ──

    #[test]
    fn parse_usage_line_extracts_full_usage_block() {
        let line = obj(
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-x","usage":{
                "input_tokens":100,"output_tokens":20,
                "cache_read_input_tokens":50,"cache_creation_input_tokens":5}}}"#,
        );
        let ev = parse_usage_line(&line, ORIGIN_SESSION)
            .expect("assistant line with usage yields an event");
        match ev {
            crate::graph::UsageEvent::Turn {
                msg_id,
                model,
                in_tok,
                out_tok,
                cache_read,
                cache_make,
                origin,
            } => {
                assert_eq!(msg_id, "m1");
                assert_eq!(model.as_deref(), Some("claude-x"));
                assert_eq!(in_tok, 100);
                assert_eq!(out_tok, 20);
                assert_eq!(cache_read, 50);
                assert_eq!(cache_make, 5);
                assert_eq!(
                    origin,
                    ORIGIN_SESSION,
                    "origin flows through from the caller"
                );
            }
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_line_tolerates_absent_usage() {
        // Older transcript lines (or a partial line mid-stream) may carry no
        // `usage` block at all: still a Turn event (so the msg_id UPSERT can
        // later overwrite it with real numbers), just with zeroed tokens.
        let line = obj(r#"{"type":"assistant","message":{"id":"m2"}}"#);
        let ev = parse_usage_line(&line, ORIGIN_SESSION)
            .expect("absent usage still yields an event");
        match ev {
            crate::graph::UsageEvent::Turn {
                msg_id,
                model,
                in_tok,
                out_tok,
                cache_read,
                cache_make,
                ..
            } => {
                assert_eq!(msg_id, "m2");
                assert_eq!(model, None);
                assert_eq!((in_tok, out_tok, cache_read, cache_make), (0, 0, 0, 0));
            }
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_line_partial_usage_defaults_missing_fields() {
        // A usage block with only some fields present (a plausible partial
        // stream update) — present fields are read, absent ones default to 0.
        let line = obj(r#"{"type":"assistant","message":{"id":"m3","usage":{"input_tokens":7}}}"#);
        let ev = parse_usage_line(&line, ORIGIN_SESSION).unwrap();
        match ev {
            crate::graph::UsageEvent::Turn {
                in_tok,
                out_tok,
                cache_read,
                cache_make,
                ..
            } => {
                assert_eq!(in_tok, 7);
                assert_eq!((out_tok, cache_read, cache_make), (0, 0, 0));
            }
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_line_none_for_non_assistant() {
        let line = obj(r#"{"type":"user","message":{"content":"hi"}}"#);
        assert!(parse_usage_line(&line, ORIGIN_SESSION).is_none());
    }

    #[test]
    fn parse_usage_line_none_without_message_id() {
        let line = obj(r#"{"type":"assistant","message":{"usage":{"input_tokens":1}}}"#);
        assert!(parse_usage_line(&line, ORIGIN_SESSION).is_none());
    }

    #[test]
    fn usage_origin_tags_the_three_line_forms() {
        // Parent-transcript line (no sidechain flag) → the caller's Session.
        let parent = obj(r#"{"type":"assistant","message":{"id":"m1"}}"#);
        assert_eq!(
            usage_origin(&parent, ORIGIN_SESSION),
            ORIGIN_SESSION
        );
        // Inline `isSidechain:true` line (1.x sub-agent) → Agent even from the
        // parent drain's Session default.
        let sidechain = obj(r#"{"type":"assistant","isSidechain":true,"message":{"id":"m2"}}"#);
        assert_eq!(
            usage_origin(&sidechain, ORIGIN_SESSION),
            ORIGIN_AGENT
        );
        // Sub-agent transcript FILE line (2.x) → the drain passes Agent; a plain
        // line there (no sidechain flag) stays Agent.
        assert_eq!(
            usage_origin(&parent, ORIGIN_AGENT),
            ORIGIN_AGENT
        );
    }

    #[test]
    fn extract_tool_results_reads_string_content() {
        let line = obj(r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"hello world"}
            ]}}"#);
        let got = extract_tool_results(&line);
        assert_eq!(
            got,
            vec![("toolu_1".to_string(), "hello world".chars().count())]
        );
    }

    #[test]
    fn extract_tool_results_sums_text_blocks_and_skips_non_text() {
        let line = obj(r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_2","content":[
                    {"type":"text","text":"abc"},
                    {"type":"image","source":{}},
                    {"type":"text","text":"de"}
                ]}
            ]}}"#);
        let got = extract_tool_results(&line);
        assert_eq!(
            got,
            vec![("toolu_2".to_string(), 5)],
            "only the two text blocks (3+2 chars) count"
        );
    }

    #[test]
    fn extract_tool_results_handles_multiple_parallel_results() {
        let line = obj(r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_a","content":"aa"},
                {"type":"tool_result","tool_use_id":"toolu_b","content":"bbbb"}
            ]}}"#);
        let got = extract_tool_results(&line);
        assert_eq!(
            got,
            vec![("toolu_a".to_string(), 2), ("toolu_b".to_string(), 4)]
        );
    }

    #[test]
    fn extract_tool_results_ignores_non_tool_result_and_non_user_lines() {
        // A real user prompt (text block, not a tool_result carrier).
        let prompt = obj(r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        assert!(extract_tool_results(&prompt).is_empty());
        // An assistant line is never a tool_result carrier.
        let assistant = obj(r#"{"type":"assistant","message":{"id":"m1","content":[]}}"#);
        assert!(extract_tool_results(&assistant).is_empty());
    }

    #[test]
    fn tool_name_ring_joins_and_evicts_beyond_cap() {
        let mut ring = ToolNameRing::default();
        ring.insert("toolu_1".to_string(), "Read".to_string());
        assert_eq!(ring.get("toolu_1"), Some("Read"));
        assert_eq!(ring.get("toolu_missing"), None);

        // Insert one more than the cap; the oldest (`toolu_1`) is evicted,
        // the newest survives.
        for i in 0..TOOL_NAME_RING_CAP {
            ring.insert(format!("toolu_gen_{i}"), "Bash".to_string());
        }
        assert_eq!(
            ring.get("toolu_1"),
            None,
            "oldest entry evicted beyond the cap"
        );
        assert_eq!(
            ring.get(&format!("toolu_gen_{}", TOOL_NAME_RING_CAP - 1)),
            Some("Bash")
        );
    }

    #[test]
    fn tool_name_ring_clear_drops_everything() {
        let mut ring = ToolNameRing::default();
        ring.insert("toolu_1".to_string(), "Read".to_string());
        ring.clear();
        assert_eq!(ring.get("toolu_1"), None);
    }

    #[test]
    fn mem_target_skips_events_with_no_usable_target() {
        // Regression (legacy sweep session 5): a Bash tool_use with a missing
        // `command` used to record a content-free mem_event (empty path, no
        // detail), wasting a ring slot — the OpenCode ingress in
        // offload::loopback already guarded this; both taps now match.
        use crate::harness::plugin::MemArg;
        let input = obj(r#"{"description":"oops, no command key"}"#);
        assert_eq!(mem_target(MemArg::Command, Some(&input)), None);
        assert_eq!(mem_target(MemArg::Path, Some(&input)), None);
        assert_eq!(mem_target(MemArg::Pattern, Some(&input)), None);
        assert_eq!(mem_target(MemArg::Command, None), None);

        let bash = obj(r#"{"command":"cargo test"}"#);
        assert_eq!(
            mem_target(MemArg::Command, Some(&bash)),
            Some((String::new(), Some("cargo test".to_string())))
        );
        let read = obj(r#"{"file_path":"src/main.rs"}"#);
        assert_eq!(
            mem_target(MemArg::Path, Some(&read)),
            Some(("src/main.rs".to_string(), None))
        );
    }

    #[test]
    fn record_usage_is_a_noop_without_graph_memory() {
        // agent_ctx()'s ctx.mem is None; record_usage must not panic, and —
        // mirroring record_tool_events's early return — must not even touch
        // the ring, since without memory there's nothing to join it into.
        let (ctx, _sig) = agent_ctx();
        let mut ring = ToolNameRing::default();
        let dir = std::env::temp_dir();
        let line = obj(r#"{"type":"assistant","message":{"id":"m1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
            ],"usage":{"input_tokens":10,"output_tokens":2}}}"#);
        record_usage(
            &line,
            &mut ring,
            &dir,
            "s1",
            &ctx,
            ORIGIN_SESSION,
        );
        assert_eq!(
            ring.get("toolu_1"),
            None,
            "mem is None, so the tap is a full no-op"
        );
    }

    // ── Sub-agent transcript tail + drift canary ──────────────────────────

    #[test]
    fn read_complete_lines_holds_back_partial_trailing_line() {
        let dir = std::env::temp_dir().join(format!("oob-subagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-x.jsonl");

        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"partial").unwrap();
        let (chunk, offset) = read_complete_lines(&path, 0).expect("two complete lines");
        assert_eq!(chunk, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(offset, chunk.len() as u64);

        // Nothing new past the partial line yet.
        assert!(read_complete_lines(&path, offset).is_none());

        // The partial line completing is picked up from the held offset.
        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"partial\":3}\n").unwrap();
        let (chunk, _) = read_complete_lines(&path, offset).expect("completed line");
        assert_eq!(chunk, "{\"partial\":3}\n");

        // A missing file is a quiet retry, not an error.
        assert!(read_complete_lines(&dir.join("gone.jsonl"), 0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_scan_is_a_noop_without_graph_memory() {
        // Every tap scan feeds is mem-gated, so without memory it must not
        // even track files — and drift_tick must stay silent too (its
        // "transcripts missing" arm would otherwise be a false constant).
        let (ctx, _sig) = agent_ctx();
        let root = std::env::temp_dir().join(format!("oob-subagent-scan-{}", uuid::Uuid::new_v4()));
        let dir = subagents_dir(&root, "s1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-a.jsonl"), "{\"type\":\"assistant\"}\n").unwrap();

        let mut subs = SubagentState {
            completion_seen: true, // even with a completed agent…
            ..SubagentState::default()
        };
        subs.scan(&root, "s1", &root, &ctx);
        assert!(
            subs.files.is_empty(),
            "mem is None, so no files are tracked"
        );
        subs.drift_tick(&root, "s1", &ctx);
        assert!(!subs.drift_reported && subs.drift_ticks == 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drift_condition_fires_on_missing_transcripts_only_after_real_completion() {
        let mut subs = SubagentState::default();
        assert_eq!(subs.drift_condition(), None, "idle session is healthy");

        // Launch alone isn't evidence — the agent may still be running.
        subs.launch_seen = true;
        assert_eq!(subs.drift_condition(), None);

        // A completed agent with traffic in neither location is drift.
        subs.completion_seen = true;
        assert!(subs
            .drift_condition()
            .unwrap()
            .contains("transcripts moved"));

        // Inline sidechain lines mean the 1.x contract is live — healthy.
        subs.sidechain_seen = true;
        assert_eq!(subs.drift_condition(), None);

        // As does a tracked subagent file under the 2.x contract.
        subs.sidechain_seen = false;
        subs.files
            .insert(PathBuf::from("agent-a.jsonl"), SubagentFile::default());
        assert_eq!(subs.drift_condition(), None);
    }

    #[test]
    fn drift_condition_fires_on_unrecognized_launcher_tool() {
        // Transcript files with no recognized launch tool_use ⇒ the launcher
        // was renamed again (as Task→Agent was).
        let mut subs = SubagentState::default();
        subs.files
            .insert(PathBuf::from("agent-a.jsonl"), SubagentFile::default());
        assert!(subs
            .drift_condition()
            .unwrap()
            .contains("launcher tool renamed"));

        // …unless we attached mid-session: the launches may predate us.
        subs.skip_backlog = true;
        assert_eq!(subs.drift_condition(), None);

        // A recognized launch silences it.
        subs.skip_backlog = false;
        subs.launch_seen = true;
        assert_eq!(subs.drift_condition(), None);
    }

    #[test]
    fn subagent_reset_clears_state_and_keeps_backlog_flag() {
        let mut subs = SubagentState {
            launch_seen: true,
            completion_seen: true,
            drift_ticks: 2,
            ..SubagentState::default()
        };
        subs.files
            .insert(PathBuf::from("agent-a.jsonl"), SubagentFile::default());
        subs.reset(true);
        assert!(subs.skip_backlog);
        assert!(subs.files.is_empty());
        assert!(!subs.launch_seen && !subs.completion_seen && subs.drift_ticks == 0);
    }

    // ── Format tolerance: the transcript is an UNSTABLE upstream contract ──

    #[test]
    fn assistant_line_with_effort_and_unknown_fields_is_read_normally() {
        // CLI 2.1.212 added `message.effort`; assume more fields will follow.
        // Every reader here walks an untyped Value, so unknown keys — at the
        // top level, inside `message`, and inside a content block — must be
        // ignored rather than failing the line.
        let line = obj(
            r#"{"type":"assistant","uuid":"u1","parentUuid":"u0","brandNewTopLevel":{"x":1},
                "message":{"id":"m1","model":"claude-x","effort":"high",
                  "stop_reason":null,"brandNewInner":[1,2],
                  "content":[
                    {"type":"thinking","thinking":"hmm","signature":"sig"},
                    {"type":"text","text":"Hello there.","brandNewBlockField":true},
                    {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.rs"}}],
                  "usage":{"input_tokens":100,"output_tokens":20,
                    "cache_read_input_tokens":50,"cache_creation_input_tokens":5,
                    "brandNewUsageField":7}}}"#,
        );

        // TTS tap: the text block is still found.
        let spoken = assistant_texts(&line);
        assert_eq!(spoken.len(), 1);
        assert_eq!(spoken[0].1, "Hello there.");

        // Usage tap: `effort` and the unknown usage key don't disturb the counts.
        let ev = parse_usage_line(&line, ORIGIN_SESSION)
            .expect("an `effort`-carrying assistant line still yields a Turn");
        assert_eq!(
            ev,
            crate::graph::UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: Some("claude-x".to_string()),
                in_tok: 100,
                out_tok: 20,
                cache_read: 50,
                cache_make: 5,
                origin: ORIGIN_SESSION.to_string(),
            }
        );

        // Agent tracking: a non-agent tool on such a line is still ignored.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let delta = update_agents(&line, &mut agents, &ctx);
        assert!(!delta.launched && !delta.completed);
        assert!(sig.try_recv().is_err());
    }

    #[test]
    fn unknown_line_types_and_new_source_variants_are_inert() {
        // `SessionStart.source` gained a `"fork"` variant (CLI 2.1.x). cImp
        // registers no SessionStart hook and deserializes no enum from the
        // transcript, so such a line simply matches none of the taps: no
        // panic, no state change, and — critically — the lines around it are
        // unaffected. This test pins that "unknown variant ⇒ inert" posture.
        let fork = obj(
            r#"{"type":"system","subtype":"session_start","source":"fork",
                "sessionId":"abc","cwd":"P:\\repo","brandNewField":42}"#,
        );
        assert!(assistant_texts(&fork).is_empty());
        assert!(parse_usage_line(&fork, ORIGIN_SESSION).is_none());
        assert!(extract_tool_results(&fork).is_empty());
        assert!(!is_user_prompt(&fork), "not a turn boundary");

        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        agents.insert("toolu_live".to_string());
        let delta = update_agents(&fork, &mut agents, &ctx);
        assert!(!delta.launched && !delta.completed);
        assert!(
            agents.contains("toolu_live"),
            "an unrecognized line must not disturb tracked agents"
        );
        assert!(sig.try_recv().is_err());

        // A wholly unknown top-level `type` behaves the same way.
        let alien = obj(r#"{"type":"some-future-record","payload":{"anything":true}}"#);
        assert!(assistant_texts(&alien).is_empty());
        assert!(parse_usage_line(&alien, ORIGIN_SESSION).is_none());
        assert!(!is_user_prompt(&alien));
    }

    #[test]
    fn parse_transcript_line_skips_corrupt_lines_only() {
        let path = Path::new("transcript.jsonl");
        assert!(parse_transcript_line(path, r#"{"type":"assistant"}"#).is_some());
        // Truncated / garbage / non-object lines are all skipped, not fatal.
        assert!(parse_transcript_line(path, r#"{"type":"assist"#).is_none());
        assert!(parse_transcript_line(path, "\u{0}\u{1}not json at all").is_none());
        assert!(parse_transcript_line(path, "{,}").is_none());
    }

    #[tokio::test]
    async fn drain_skips_a_corrupt_line_and_keeps_processing_later_lines() {
        // Regression guard for the unstable-format contract: one unreadable
        // line (a truncated write, an unknown binary blob, a future framing)
        // must not abort the tail. The launch on line 1 is only cleared by the
        // tool_result on line 4 — if the corrupt line 2 killed the drain, the
        // agent would stay outstanding and no inactive edge would arrive.
        let dir = std::env::temp_dir().join(format!("oob-corrupt-line-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-1.jsonl");
        // No `version` key anywhere: `note_cli_version` must stay out of the
        // global harness-version file during tests.
        let content = concat!(
            r#"{"type":"assistant","message":{"id":"m1","effort":"high","content":[{"type":"tool_use","id":"toolu_1","name":"Agent","input":{}}]}}"#,
            "\n",
            "{\"type\":\"assistant\",\"message\":{ TRUNCATED MID-WRITE",
            "\n",
            r#"{"type":"system","subtype":"session_start","source":"fork"}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}"#,
            "\n",
        );
        std::fs::write(&path, content).unwrap();

        let (ctx, mut sig) = agent_ctx();
        let mut seen = HashSet::new();
        let mut agents = HashSet::new();
        let mut tool_names = ToolNameRing::default();
        let mut commit_calls = IdRing::default();
        let mut version_noted = false;
        let mut subs = SubagentState::default();
        let mut turn = TurnText::default();
        let drained = drain_new_lines(
            &path,
            0,
            &mut seen,
            &mut turn,
            &mut agents,
            &mut tool_names,
            &mut commit_calls,
            &mut version_noted,
            &mut subs,
            &dir,
            "session-1",
            &ctx,
        )
        .await;

        assert_eq!(
            drained.offset,
            content.len() as u64,
            "the whole chunk is consumed despite the corrupt line"
        );
        // H-2: none of these lines carries a `sessionId`, so none of them is
        // live-session evidence — and the offset advanced anyway. The two facts
        // are independent by construction (see `Drained`).
        assert!(
            !drained.own_record,
            "a chunk with no sessionId-bearing line is not evidence, however many bytes it moved"
        );
        assert!(
            agents.is_empty(),
            "the tool_result AFTER the corrupt line still cleared the agent"
        );
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: true, .. })
        ));
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::SubagentsActiveChanged { active: false, .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── H-2 (2026-08-08) — live-session evidence is a DECODE proof ──────────

    /// The predicate itself, at the four shapes that decide it.
    ///
    /// H-2 in one sentence: the old bar was "the offset moved", which
    /// `read_complete_lines` clears for **any** newline-terminated bytes — so
    /// `echo {} > <newest>.jsonl` from the model's own shell reported a forged
    /// session live. The bar is now "a decoded record named this session".
    #[test]
    fn record_names_session_matches_only_a_line_that_names_this_session() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        // The `echo {}` PoC: valid JSON, newline-terminated, no `sessionId`.
        assert!(!record_names_session(&v("{}"), "aaaa"));
        // A REAL transcript line that legitimately lacks the field. Not
        // evidence — and, just as important, not a veto: the drain keeps going
        // and the next line may still confirm.
        assert!(!record_names_session(
            &v(r#"{"type":"file-history-snapshot","messageId":"m1"}"#),
            "aaaa"
        ));
        // Someone else's session id.
        assert!(!record_names_session(
            &v(r#"{"type":"user","sessionId":"bbbb"}"#),
            "aaaa"
        ));
        // The real thing: the stem of `<stem>.jsonl` appears as `sessionId`.
        assert!(record_names_session(
            &v(r#"{"type":"assistant","sessionId":"aaaa","message":{"id":"m1"}}"#),
            "aaaa"
        ));
        // A `file_stem` that fell back to `""` must never be confirmable, and a
        // non-object line answers false rather than panicking (the module's
        // format-tolerance discipline).
        assert!(!record_names_session(&v(r#"{"sessionId":""}"#), ""));
        assert!(!record_names_session(&v("3"), "aaaa"));
        assert!(!record_names_session(&v("[]"), "aaaa"));
    }

    /// The same rule **through the drain**, which is where the evidence is
    /// actually produced — and with the offset asserted beside it, because H-2
    /// is precisely the two facts having been one.
    #[tokio::test]
    async fn drain_reports_live_session_evidence_only_for_a_matching_session_id() {
        let dir = std::env::temp_dir().join(format!("oob-h2-evidence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // No `version` key anywhere: `note_cli_version` must stay out of the
        // global harness-version file during tests.
        let drain = |name: &str, body: &'static str, session: &'static str| {
            let path = dir.join(name);
            let parent = dir.clone();
            std::fs::write(&path, body).unwrap();
            async move {
                let (ctx, _sig) = agent_ctx();
                let mut seen = HashSet::new();
                let mut agents = HashSet::new();
                let mut tool_names = ToolNameRing::default();
                let mut commit_calls = IdRing::default();
                let mut version_noted = false;
                let mut subs = SubagentState::default();
                let mut turn = TurnText::default();
                let out = drain_new_lines(
                    &path,
                    0,
                    &mut seen,
                    &mut turn,
                    &mut agents,
                    &mut tool_names,
                    &mut commit_calls,
                    &mut version_noted,
                    &mut subs,
                    &parent,
                    session,
                    &ctx,
                )
                .await;
                (out, body.len() as u64)
            }
        };

        // Everything a forger can produce with one shell command, plus the one
        // real line that carries no `sessionId`. None of it is evidence — and
        // the offset still consumes all of it.
        let forged = concat!(
            "{}\n",
            r#"{"type":"file-history-snapshot","messageId":"m1"}"#,
            "\n",
            r#"{"type":"user","sessionId":"someone-else","message":{"content":"hi"}}"#,
            "\n",
            "not json at all, just bytes and a newline\n",
        );
        let (out, len) = drain("aaaa.jsonl", forged, "aaaa").await;
        assert!(
            !out.own_record,
            "no line named this session, so nothing here may report it live"
        );
        assert_eq!(
            out.offset, len,
            "the offset advances for every complete line regardless — H-2 is these two \
             facts being independent"
        );

        // Unparseable garbage must not confirm AND must not stop the good line
        // after it from confirming (`parse_transcript_line`'s skip-and-continue
        // contract).
        let real = concat!(
            "{\"type\":\"assistant\",\"message\":{ TRUNCATED MID-WRITE\n",
            r#"{"type":"assistant","sessionId":"bbbb","message":{"id":"m9","content":[]}}"#,
            "\n",
        );
        let (out, len) = drain("bbbb.jsonl", real, "bbbb").await;
        assert!(
            out.own_record,
            "a decoded record naming this session IS the proof, even behind a corrupt line"
        );
        assert_eq!(out.offset, len);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- V39 review HIGH-1: the completion is per TURN ----------------------

    /// A `ctx` on a tab of this test's own, so the process-global delegation
    /// registry is not shared with the canary suite's fixture replays.
    fn turn_ctx(tab: &str) -> OobContext {
        let (tts_tx, _tts_rx) = mpsc::channel::<TtsRequest>(64);
        let (sig_tx, _sig_rx) = mpsc::channel::<StateSignal>(64);
        let defaults = Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());
        OobContext {
            tab: TabId::from_str(tab),
            tts: tts_tx,
            state_signals: sig_tx,
            settings,
            cancel: CancellationToken::new(),
            mem: None,
            pushes: None,
        }
    }

    async fn drain_for_turn(
        path: &Path,
        dir: &Path,
        offset: u64,
        turn: &mut TurnText,
        ctx: &OobContext,
    ) -> u64 {
        let mut seen = HashSet::new();
        let mut agents = HashSet::new();
        let mut tool_names = ToolNameRing::default();
        let mut commit_calls = IdRing::default();
        let mut version_noted = false;
        let mut subs = SubagentState::default();
        drain_new_lines(
            path,
            offset,
            &mut seen,
            turn,
            &mut agents,
            &mut tool_names,
            &mut commit_calls,
            &mut version_noted,
            &mut subs,
            dir,
            "session-1",
            ctx,
        )
        .await
        .offset
    }

    /// **The turn boundary itself**, at every shape the transcript produces.
    #[test]
    fn only_a_non_tool_stop_reason_ends_a_turn() {
        let line = |sr: &str| {
            obj(&format!(
                r#"{{"type":"assistant","message":{{"id":"m1","stop_reason":{sr},"content":[]}}}}"#
            ))
        };
        assert!(
            !is_turn_end(&line(r#""tool_use""#)),
            "a tool call continues the turn"
        );
        assert!(is_turn_end(&line(r#""end_turn""#)));
        assert!(is_turn_end(&line(r#""max_tokens""#)));
        assert!(
            is_turn_end(&line(r#""some_future_reason""#)),
            "an unrecognized reason ends the turn: filing one message early beats hanging"
        );
        assert!(!is_turn_end(&line("null")), "present-but-null claims nothing");
        assert!(!is_turn_end(&line(r#""   ""#)));
        assert!(!is_turn_end(&obj(r#"{"type":"user","message":{"content":"hi"}}"#)));
    }

    /// **One completion per turn, and it is the turn's LAST assistant text.**
    ///
    /// The failing shape: a preamble, a tool call, then the answer. Per-message
    /// filing handed the driver "I will read that file first." and released the
    /// worker's slot while it was still working.
    #[tokio::test]
    async fn a_claude_turn_files_one_completion_and_it_is_the_final_text() {
        let dir = std::env::temp_dir().join(format!("oob-turn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-1.jsonl");
        let head = concat!(
            r#"{"type":"user","message":{"content":"summarise latch.ts"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","stop_reason":"tool_use","content":[{"type":"text","text":"Reading that file first."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#,
            "\n",
        );
        // The same API message, written as two lines: thinking first, then the
        // answer, both carrying `end_turn`.
        let tail = concat!(
            r#"{"type":"assistant","message":{"id":"m2","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"..."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","stop_reason":"end_turn","content":[{"type":"text","text":"It exports three symbols."}]}}"#,
            "\n",
        );
        let ctx = turn_ctx("ai-high1-claude");
        let worker = TabId::from_str("ai-high1-claude");
        let _registry = crate::delegation::testing::lock_registry();
        crate::delegation::testing::claim_and_submit(&worker);
        let mut turn = TurnText::default();

        std::fs::write(&path, head).unwrap();
        let offset = drain_for_turn(&path, &dir, 0, &mut turn, &ctx).await;
        assert!(
            crate::delegation::testing::take(&worker).is_none(),
            "a mid-turn preamble must NOT file a completion"
        );

        std::fs::write(&path, format!("{head}{tail}")).unwrap();
        drain_for_turn(&path, &dir, offset, &mut turn, &ctx).await;
        assert_eq!(
            crate::delegation::testing::take(&worker).as_deref(),
            Some("It exports three symbols."),
            "the completion is the turn's final text, not the `end_turn` line before it"
        );
        assert!(
            crate::delegation::testing::take(&worker).is_none(),
            "filed exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A turn ending and the next prompt in ONE drain pass still files**
    /// (V39 review R-2).
    ///
    /// The fire is deferred to the end of the pass, and the user prompt that
    /// starts the next turn resets the buffer — so a pass carrying both wiped
    /// an ended-but-unfiled turn, and the delegation waiting on it ran to its
    /// deadline. Routine whenever the user types quickly or a paused tap
    /// catches up in one read.
    #[tokio::test]
    async fn a_turn_that_ends_in_the_same_pass_as_the_next_prompt_still_files() {
        let dir = std::env::temp_dir().join(format!("oob-turn-r2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-1.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"content":"summarise latch.ts"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"text","text":"It exports three symbols."}]}}"#,
                "\n",
                // …and the user's next prompt lands in the SAME chunk.
                r#"{"type":"user","message":{"content":"now the other file"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let ctx = turn_ctx("ai-r2-worker");
        let worker = TabId::from_str("ai-r2-worker");
        let _registry = crate::delegation::testing::lock_registry();
        crate::delegation::testing::claim_and_submit(&worker);
        let mut turn = TurnText::default();
        drain_for_turn(&path, &dir, 0, &mut turn, &ctx).await;
        assert_eq!(
            crate::delegation::testing::take(&worker).as_deref(),
            Some("It exports three symbols."),
            "the answer must be filed before the next prompt resets the buffer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A turn that ends with no text files an EMPTY completion** (V39 review
    /// R-9, locked decision 13).
    ///
    /// The engine turns that into `worker produced no text` immediately. Filing
    /// nothing made the same outcome arrive as a `timeout` ten minutes later,
    /// claiming the worker was still running when it had stopped.
    #[tokio::test]
    async fn a_turn_that_says_nothing_files_an_empty_completion() {
        let dir = std::env::temp_dir().join(format!("oob-turn-r9-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-1.jsonl");
        let body = concat!(
            r#"{"type":"user","message":{"content":"summarise latch.ts"}}"#,
            "\n",
            // The turn ends carrying nothing but reasoning.
            r#"{"type":"assistant","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"..."}]}}"#,
            "\n",
        );
        std::fs::write(&path, body).unwrap();
        let ctx = turn_ctx("ai-r9-worker");
        let worker = TabId::from_str("ai-r9-worker");
        let _registry = crate::delegation::testing::lock_registry();
        crate::delegation::testing::claim_and_submit(&worker);
        let mut turn = TurnText::default();

        let offset = drain_for_turn(&path, &dir, 0, &mut turn, &ctx).await;
        assert!(
            crate::delegation::testing::take(&worker).is_none(),
            "one pass of grace: the turn's `text` line may still be coming"
        );
        // The next poll finds nothing new — which is what a finished turn looks
        // like — and the grace runs out.
        drain_for_turn(&path, &dir, offset, &mut turn, &ctx).await;
        assert_eq!(
            crate::delegation::testing::take(&worker).as_deref(),
            Some(""),
            "an empty completion, so the driver is told `no text` now rather than `timeout` later"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// …and the grace is load-bearing: the text line arriving in the NEXT pass
    /// still wins over the empty completion.
    #[tokio::test]
    async fn a_turns_final_text_arriving_a_pass_late_still_wins() {
        let dir = std::env::temp_dir().join(format!("oob-turn-r9b-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-1.jsonl");
        let head = concat!(
            r#"{"type":"user","message":{"content":"summarise latch.ts"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"..."}]}}"#,
            "\n",
        );
        let tail = concat!(
            r#"{"type":"assistant","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"text","text":"It exports three symbols."}]}}"#,
            "\n",
        );
        std::fs::write(&path, head).unwrap();
        let ctx = turn_ctx("ai-r9b-worker");
        let worker = TabId::from_str("ai-r9b-worker");
        let _registry = crate::delegation::testing::lock_registry();
        crate::delegation::testing::claim_and_submit(&worker);
        let mut turn = TurnText::default();

        let offset = drain_for_turn(&path, &dir, 0, &mut turn, &ctx).await;
        assert!(crate::delegation::testing::take(&worker).is_none());
        std::fs::write(&path, format!("{head}{tail}")).unwrap();
        drain_for_turn(&path, &dir, offset, &mut turn, &ctx).await;
        assert_eq!(
            crate::delegation::testing::take(&worker).as_deref(),
            Some("It exports three symbols."),
            "the real answer, not the empty completion the grace was holding back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sub-agent's own `end_turn` is not the tab's turn ending.
    #[tokio::test]
    async fn a_sidechain_turn_does_not_complete_a_delegation() {
        let dir = std::env::temp_dir().join(format!("oob-turn-side-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-1.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"content":"go"}}"#,
                "\n",
                r#"{"type":"assistant","isSidechain":true,"message":{"id":"s1","stop_reason":"end_turn","content":[{"type":"text","text":"sub-agent done"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let ctx = turn_ctx("ai-high1-side");
        let worker = TabId::from_str("ai-high1-side");
        let _registry = crate::delegation::testing::lock_registry();
        crate::delegation::testing::claim_and_submit(&worker);
        let mut turn = TurnText::default();
        drain_for_turn(&path, &dir, 0, &mut turn, &ctx).await;
        assert!(
            crate::delegation::testing::take(&worker).is_none(),
            "a sub-agent's message is not the worker tab's answer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

}

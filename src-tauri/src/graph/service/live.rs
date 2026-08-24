//! The **live-session registry** — V24 Phase B, V28 (#13), H1, V34, V40 Phase D.
//!
//! Split out of [`super`] by V42 R6 (#117) as pure code motion. Like the read
//! advisor beside it, this subsystem was already a free-function island with a
//! value-type seam: every decision — the active set, the tab→session lookup,
//! the co-tenancy ambiguity predicate, TTL eviction — is a pure function of a
//! [`LiveRegistry`] plus the [`LiveTabRoot`] map, and only the six
//! [`GraphService`] methods here hold the locks that produce them.
//!
//! The two maps themselves stay on `GraphService` (`live_sessions`,
//! `live_tab_roots`): a child module already sees its parent's private fields,
//! so the split opened no state. What widened, all to `pub(super)`, is the five
//! items the parent still names — [`LiveRegistry`] (a field's type), its
//! [`LiveKey`]/[`LiveSession`] halves, [`LiveTabRoot`] (the other field's
//! type), and [`compute_active_session_ids`] (called by
//! `GraphService::active_session_ids`, which stays with `usage_snapshot`) —
//! plus [`LIVE_SESSION_TTL_MS`] and [`tab_binding_is_ambiguous`], which those
//! fields' doc comments link to.
//!
//! **No harness identity lives here** (V40 locked decision 10(a)): the registry
//! left `harness/layering.rs`'s IDENTITY_ALLOWLIST in V40 Phase D, and this file
//! inherits that — `LiveKey` carries the plugin's declared `SessionKey` space
//! and `live_sessions_for` takes a `HarnessId`, so no built-in id is spelled.

use super::*;

/// V24 Phase B: a registry entry marks a session live within
/// [`LIVE_SESSION_TTL_MS`] of its last refresh (a still-ticking Claude drain
/// tick, or a still-reporting OpenCode session). The Claude drain polls every
/// ~200ms, so even an idle-but-open tab refreshes well inside this window; the
/// generous margin only tolerates a slow drain of a large transcript.
///
/// H1-R2 (2026-08-05 review): the margin is NOT self-evident for a busy tab, and
/// the failure mode is worse than for an idle one. The Claude tap's drain can
/// park for minutes inside `ctx.speak()` (a bounded TTS channel drained at ONNX
/// synthesis speed), so "the loop polls every 200ms" describes only the idle
/// case. A tab whose entries aged out here does not merely go quiet: its
/// co-tenant stops being detected, [`tab_binding_is_ambiguous`] flips to `false`
/// for the sibling, and the sibling's tap — which tails the *stalled* tab's
/// transcript, the newest file — gains a CONFIDENT and WRONG session binding.
/// The tap therefore refreshes both of its entries from an independent heartbeat
/// task (`harness::claude::read::TapHeartbeat`) that no drain-side await can starve; this
/// TTL only has to outlast that heartbeat's cadence by a wide margin.
pub(super) const LIVE_SESSION_TTL_MS: i64 = 90_000;

/// V24 Phase B: the recency half of the decided "open tabs + recency"
/// semantics — a session whose last recorded activity falls within this window
/// also counts as active, catching a live session the registry missed (a
/// pre-existing tab from before this process, or the gap before the first
/// drain tick).
const LIVE_SESSION_RECENCY_MS: i64 = 5 * 60_000;

/// V24 Phase B: one live-session registry entry. `session_id` is the value (not
/// the key) so a Claude tab keyed by its stable tab id can rotate the session
/// it reports without leaking a stale key; OpenCode keys by the reporting
/// session id itself (no tab binding on the loopback path).
#[derive(Clone, Debug)]
pub(super) struct LiveSession {
    /// Which agent reported the entry (`"claude"` / `"opencode"`). Read by
    /// [`GraphService::live_sessions_for`] — the NC-2 permission-hook's
    /// session→tab mapping only trusts Claude entries, whose key IS a tab id.
    agent: String,
    session_id: String,
    last_seen_ms: i64,
}

/// **A live-session registry key: the space it lives in, and the id.**
///
/// V40 Phase D, locked decision 20. This map used to be keyed by a bare
/// `String` holding *either* a cImp tab id (a harness whose session cImp's own
/// reader binds — [`SessionKey::Tab`]) *or* a session id the harness reported
/// over the loopback ([`SessionKey::Session`]). One map, two key spaces: a
/// `/memory/event` naming a configured tab id landed on that tab's entry and
/// repointed its session, which flapped the taint latch clear in a loop (C-2).
/// It was closed by refusing any body-supplied key that named a configured tab
/// — a check beside the write, of a list the check had to keep in step with.
///
/// The spaces are separate now, so the collision cannot be expressed: a
/// `/memory/event` writes into the session space, every tab-keyed reader looks
/// in the tab space, and no string can be in both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct LiveKey {
    space: crate::harness::plugin::SessionKey,
    id: String,
}

impl LiveKey {
    /// A key in the TAB space — cImp's own reader, keyed by the tab it runs in.
    fn tab(id: &str) -> Self {
        Self {
            space: crate::harness::plugin::SessionKey::Tab,
            id: id.to_string(),
        }
    }

    /// A key in the SESSION space — an id the harness reported for itself.
    fn session(id: &str) -> Self {
        Self {
            space: crate::harness::plugin::SessionKey::Session,
            id: id.to_string(),
        }
    }
}

/// The live-session registry: [`LiveKey`] → what that key last reported.
pub(super) type LiveRegistry = HashMap<LiveKey, LiveSession>;

/// H1 fix (2026-08-05 review): one RUNNING agent tab and the transcript source
/// it binds its session identity from — for Claude, the
/// `~/.claude/projects/<slug>/` directory its out-of-band tap tails.
///
/// The Claude tap has no per-process discriminator: it binds to the
/// newest-mtime `*.jsonl` under that directory, so TWO running Claude tabs on
/// one project (e.g. the built-in `claude` + `claude-local`, both `cwd: None`)
/// both resolve to whichever session wrote last. Every identity claim keyed by
/// such a tab is therefore unprovable. This map is what makes that condition
/// *detectable* at the registry seam: it is written by the tap itself (so it
/// reflects tabs that are genuinely running, not merely configured), keyed by
/// the stable tab id, refreshed on every poll tick, TTL-expired like
/// [`LiveSession`], and cleared by the tap's RAII guard on tab exit.
#[derive(Clone, Debug)]
pub(super) struct LiveTabRoot {
    /// Which harness runs in this tab (`"claude"`). Only agents whose binding
    /// is root-derived register here — OpenCode binds per-tab off its own SSE
    /// stream and is deliberately absent (see [`tab_binding_is_ambiguous`]).
    agent: String,
    /// The transcript source directory the tap tails, as a normalized
    /// COMPARISON KEY ([`crate::fsutil::norm_dir_key_path`]) — not a
    /// displayable path. H1-R5: normalized once here, at the single write site
    /// ([`upsert_live_tab_root`]), so every reader compares canonical keys and
    /// two tabs whose hand-set cwds differ only by case or a trailing separator
    /// are still recognized as co-tenants. Same normalization posture as the
    /// permission hook's cwd fallback (`offload::loopback::norm_dir`), which
    /// routes through the same helper.
    root: PathBuf,
    last_seen_ms: i64,
    /// V34: this tab's session id was PINNED by cImp at spawn (`--session-id`),
    /// so its tap follows one known transcript file rather than whichever is
    /// newest under `root`.
    ///
    /// This is what retires the V28 decision-4a degradation for the tab: the
    /// co-tenancy that makes newest-wins unprovable says nothing about a tab
    /// that never consults "newest" in the first place. See
    /// [`tab_binding_is_ambiguous`].
    pinned: bool,
}

/// V24 Phase B (pure): the "open tabs + recency" active-session set — a session
/// is active when it has recent activity (`last_ms` within
/// [`LIVE_SESSION_RECENCY_MS`]) OR a fresh registry entry (`last_seen_ms` within
/// [`LIVE_SESSION_TTL_MS`]). The registry (`live`) is process-wide, so its
/// contribution is intersected with `sessions` (the queried root's known
/// sessions) to avoid leaking another project's live session into this
/// snapshot — a fresh entry whose session isn't in `sessions` has no row to
/// mark here anyway. Deduped; sorted for a stable payload. Free-standing so it's
/// unit-testable without an `AppHandle`.
pub(super) fn compute_active_session_ids(
    live: &LiveRegistry,
    sessions: &[SessionUsageRow],
    now: i64,
) -> Vec<String> {
    let known: HashSet<&str> = sessions.iter().map(|r| r.session_id.as_str()).collect();
    let mut active: HashSet<String> = HashSet::new();
    // Recency half: any known session touched within the window.
    for r in sessions {
        if now.saturating_sub(r.last_ms) <= LIVE_SESSION_RECENCY_MS {
            active.insert(r.session_id.clone());
        }
    }
    // Registry half: fresh entries whose session belongs to this root.
    for e in live.values() {
        if now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS
            && known.contains(e.session_id.as_str())
        {
            active.insert(e.session_id.clone());
        }
    }
    let mut out: Vec<String> = active.into_iter().collect();
    out.sort();
    out
}

/// H1 fix (pure): is `tab`'s session binding UNPROVABLE because another RUNNING
/// tab of the same agent tails the same transcript root?
///
/// **The single implementation of the ambiguity predicate.** Every consumer of a
/// tab-keyed identity claim routes through it, so graph/memory scoping and
/// permission-hook attribution can never disagree about who is ambiguous.
///
/// Semantics:
///  * `false` when `tab` has no entry — an agent that does NOT bind by root
///    (OpenCode: per-tab SSE with the session id on the wire) never registers
///    here, so it is never degraded; likewise a tap that could not resolve a
///    root (no home dir) never marked a session either.
///  * `false` when exactly one running tab holds that root — the overwhelmingly
///    common case, unchanged.
///  * `true` from the moment a second running tab of the same agent registers
///    the same root, for as long as both entries are fresh.
///
/// TTL-filtered on both sides so a leaked/never-cleared entry cannot disable
/// scoping forever, and self-comparison is excluded by key. Free-standing so it
/// is unit-testable without an `AppHandle`.
pub(super) fn tab_binding_is_ambiguous(
    roots: &HashMap<String, LiveTabRoot>,
    tab: &str,
    agent: &str,
    now: i64,
) -> bool {
    let fresh = |e: &LiveTabRoot| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS;
    let Some(mine) = roots.get(tab).filter(|e| e.agent == agent).filter(|e| fresh(e)) else {
        return false;
    };
    // V34: a PINNED tab is never ambiguous, however many co-tenants share its
    // root. Ambiguity was only ever a property of the newest-wins binding — two
    // taps racing for the same "newest `*.jsonl`" — and a pinned tap does not
    // consult newest at all: cImp generated the session id, passed it on the
    // child's own command line, and the tap follows that one file. A co-tenant
    // cannot make an id we chose ourselves wrong.
    //
    // Deliberately asymmetric: this checks only `mine`. An UNPINNED tab stays
    // ambiguous whenever any same-root co-tenant is running, pinned or not,
    // because it is still the one doing the guessing — its tap can latch onto
    // the pinned tab's transcript just as easily as onto another guesser's.
    if mine.pinned {
        return false;
    }
    roots
        .iter()
        .any(|(k, e)| k != tab && e.agent == agent && e.root == mine.root && fresh(e))
}

/// V28: the live session id reported by `tab`, or `None`. Pure half of
/// [`GraphService::live_session_for_tab`] so it can be unit-tested without an
/// `AppHandle`.
///
/// Deliberately strict, matching NC-2's resolver discipline: an EXACT key match
/// (no prefix/fuzzy tab matching), the entry's `agent` must equal the calling
/// agent (a tab id could in principle be reused across harnesses), and the entry
/// must still be inside [`LIVE_SESSION_TTL_MS`]. Anything else returns `None` —
/// the caller then falls back to today's most-recent-session behavior rather
/// than attributing a call to a session it can't prove.
///
/// H1 fix: also `None` when [`tab_binding_is_ambiguous`] holds — with two
/// running Claude tabs on one project the registry's answer is whichever session
/// wrote last, for BOTH tabs, so honoring it would put tab A's memory writes in
/// tab B's scope. Degrading to unscoped (V28 decision 4's documented fail-open)
/// is strictly better than a confidently wrong scope.
fn lookup_live_session_for_tab(
    live: &LiveRegistry,
    roots: &HashMap<String, LiveTabRoot>,
    tab: &str,
    agent: &str,
    now: i64,
) -> Option<String> {
    if tab_binding_is_ambiguous(roots, tab, agent, now) {
        return None;
    }
    live.get(&LiveKey::tab(tab))
        .filter(|e| e.agent == agent)
        .filter(|e| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS)
        .map(|e| e.session_id.clone())
}

/// NC-2 + H1 fix (pure): the `(tab_id, session_id)` pairs the permission-hook
/// resolver may trust — every fresh CLAUDE registry entry MINUS the tabs whose
/// binding is ambiguous ([`tab_binding_is_ambiguous`]).
///
/// Dropping the pair (rather than the whole candidate) is what makes
/// `resolve_permission_tab` REFUSE instead of guess: with no session to match
/// on, its session/transcript passes find nothing, and its last-resort `cwd`
/// pass sees the ≥2 same-root tabs and declines too. That also closes the
/// launch-order window in which the registry held tab A → tab B's *fresh*
/// session **uniquely** (A's tap rotates onto B's new file and marks it live
/// before B's own tap confirms) — during that window both tabs are running on
/// one root, so the predicate is already true.
///
/// Pure half of [`GraphService::live_sessions_for`].
///
/// V40 Phase D (locked decision 20): `agent` is an argument. This was
/// `live_claude_tab_sessions`, with `"claude"` written into both filters — so
/// the permission-edge resolver could only ever resolve one harness's prompts,
/// and every other harness's fell back to the cwd pass or was dropped.
fn live_tab_sessions(
    live: &LiveRegistry,
    roots: &HashMap<String, LiveTabRoot>,
    agent: &str,
    now: i64,
) -> Vec<(String, String)> {
    live.iter()
        // TAB space only: the answer is a `(tab_id, session_id)` pair, and a
        // session-space entry has no tab to name.
        .filter(|(k, _)| k.space == crate::harness::plugin::SessionKey::Tab)
        .filter(|(_, e)| {
            e.agent == agent && now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS
        })
        .filter(|(k, _)| !tab_binding_is_ambiguous(roots, &k.id, agent, now))
        .map(|(k, e)| (k.id.clone(), e.session_id.clone()))
        .collect()
}

/// H1 fix (pure): upsert the `tab` → transcript-root claim and stamp it fresh at
/// `now`, then evict entries whose last refresh has aged past
/// [`LIVE_SESSION_TTL_MS`] (a tap that died without running its RAII guard must
/// not leave a phantom co-tenant suppressing scoping forever).
///
/// **The single write site for [`LiveTabRoot`]**, and therefore the one place
/// the root is normalized (H1-R5): stored as a comparison key via
/// [`crate::fsutil::norm_dir_key_path`], so [`tab_binding_is_ambiguous`]'s
/// equality test — and any future reader — can never be defeated by two spellings
/// of one directory. Free-standing so it's unit-testable without an `AppHandle`.
fn upsert_live_tab_root(
    roots: &mut HashMap<String, LiveTabRoot>,
    tab: &str,
    agent: &str,
    root: &Path,
    pinned: bool,
    now: i64,
) {
    let key = crate::fsutil::norm_dir_key_path(root);
    // Entry API so the steady-state refresh (drain tick + heartbeat) only
    // stamps `last_seen_ms` in place.
    roots
        .entry(tab.to_string())
        .and_modify(|e| {
            e.last_seen_ms = now;
            e.agent = agent.to_string();
            e.root = key.clone();
            // Kept current rather than sticky: a pinned tap DROPS its pin if
            // the harness never wrote the pinned transcript (see
            // `harness::claude::read`'s pin grace), and the tab must degrade back to
            // ambiguous with it.
            e.pinned = pinned;
        })
        .or_insert_with(|| LiveTabRoot {
            agent: agent.to_string(),
            root: key,
            last_seen_ms: now,
            pinned,
        });
    roots.retain(|_, e| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS);
}

/// V24 Phase B: drop live-session registry entries older than
/// [`LIVE_SESSION_TTL_MS`] — the registry half's cutoff. Called opportunistically
/// from [`GraphService::mark_live_session`] so the map doesn't grow without bound
/// (OpenCode keys have no cancel signal on the loopback path, so TTL is their
/// only reclamation). Safe because a TTL-stale entry is already ignored by
/// [`compute_active_session_ids`]'s registry half, and the recency half (the
/// younger [`LIVE_SESSION_RECENCY_MS`] window over recorded activity) covers any
/// session that is still genuinely active — so eviction can never change an
/// active-set result. Free-standing so it's unit-testable without an `AppHandle`.
fn evict_stale_live_sessions(live: &mut LiveRegistry, now: i64) {
    live.retain(|_, e| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS);
}

impl GraphService {
    /// V24 Phase B: upsert a live-session registry entry, stamping
    /// `last_seen_ms` to now. Called on every reader drain tick and every
    /// `/memory/event` — cheap and idempotent.
    ///
    /// **`space` is not a formality** (V40 Phase D, locked decision 20). It
    /// says whether `key` is a cImp TAB id — which only cImp's own reader may
    /// claim — or a SESSION id the harness reported for itself. The two live in
    /// separate spaces, so a value that arrived over the wire can no longer
    /// name a tab and repoint its session (C-2); the collision check that used
    /// to guard that is gone because the collision is now unrepresentable.
    pub fn mark_live_session(
        &self,
        space: crate::harness::plugin::SessionKey,
        key: &str,
        agent: &str,
        session_id: &str,
    ) {
        let now = crate::activity::now_ms() as i64;
        let key = match space {
            crate::harness::plugin::SessionKey::Tab => LiveKey::tab(key),
            crate::harness::plugin::SessionKey::Session => LiveKey::session(key),
        };
        if let Ok(mut m) = self.live_sessions.lock() {
            // Entry API so the steady-state 200ms Claude drain tick only stamps
            // `last_seen_ms` in place — no per-tick allocation of a fresh entry.
            m.entry(key)
                .and_modify(|e| {
                    e.last_seen_ms = now;
                    // The reported session/agent can rotate under a stable key
                    // (a Claude tab keyed by its tab id) — keep them current.
                    e.session_id = session_id.to_string();
                    e.agent = agent.to_string();
                })
                .or_insert_with(|| LiveSession {
                    agent: agent.to_string(),
                    session_id: session_id.to_string(),
                    last_seen_ms: now,
                });
            // Opportunistic eviction so OpenCode session keys — which have no
            // cancel signal on the loopback path — can't accumulate forever.
            evict_stale_live_sessions(&mut m, now);
        }
    }

    /// V24 Phase B: drop a live-session registry entry by TAB id — a reader
    /// calls this on tab cancel so a closed tab stops being reported active
    /// before its TTL lapses. Session-space entries have no tab binding to
    /// cancel and rely on TTL expiry alone.
    ///
    /// H1 fix: also drops the tab's [`LiveTabRoot`] — the two facts have exactly
    /// one lifetime (this tab's tap is running), and clearing them together is
    /// what lets a *closed* second tab stop suppressing the survivor's scoping
    /// immediately rather than after the TTL. A no-op for keys that never
    /// registered a root (every OpenCode key).
    pub fn clear_live_session(&self, key: &str) {
        if let Ok(mut m) = self.live_sessions.lock() {
            m.remove(&LiveKey::tab(key));
        }
        if let Ok(mut m) = self.live_tab_roots.lock() {
            m.remove(key);
        }
    }

    /// H1 fix: record that the tab keyed `tab` is RUNNING `agent` and binds its
    /// session identity from the transcript source `root` — see [`LiveTabRoot`]
    /// and [`tab_binding_is_ambiguous`]. Called from the tab's out-of-band tap
    /// on every poll tick (cheap, idempotent, keeps the entry inside
    /// [`LIVE_SESSION_TTL_MS`]); cleared by the tap's RAII guard via
    /// [`Self::clear_live_session`].
    ///
    /// Only agents whose binding is root-derived call this: registering an entry
    /// is what makes a tab *eligible* to be found ambiguous, so an agent that
    /// binds correctly per-tab (OpenCode) must stay absent.
    ///
    /// H1-R2: also called on a fixed cadence by the tap's heartbeat, so a drain
    /// loop parked in TTS backpressure can't let the claim age out (see
    /// [`LIVE_SESSION_TTL_MS`]). Idempotent and cheap either way; the decision
    /// (including the H1-R5 key normalization) lives in [`upsert_live_tab_root`].
    /// V34: `pinned` says this tab's tap follows a session id cImp chose and
    /// passed to the child (`--session-id`), rather than whichever transcript is
    /// newest under `root`. Passed on every tick rather than latched, so a tap
    /// that gives up on its pin degrades the tab back to ambiguous.
    pub fn mark_live_tab_root(&self, tab: &str, agent: &str, root: &Path, pinned: bool) {
        let now = crate::activity::now_ms() as i64;
        if let Ok(mut m) = self.live_tab_roots.lock() {
            upsert_live_tab_root(&mut m, tab, agent, root, pinned, now);
        }
    }

    /// NC-2 (issue #5): the live-session registry entries reported by
    /// `harness`'s tabs — `(tab_id, session_id)` per entry still inside
    /// [`LIVE_SESSION_TTL_MS`]. This is the session→tab mapping the
    /// permission-edge resolver matches a payload with: a tab-space entry is
    /// keyed by its stable TAB ID (see [`Self::mark_live_session`]) and carries
    /// the session id that tab's reader last saw, which is exactly the session
    /// id a hook payload names.
    ///
    /// **V40 Phase D (locked decision 20): the harness is an argument.** It was
    /// `live_claude_sessions`, filtering on the literal `"claude"`, so the
    /// resolver could resolve exactly one harness's prompts and silently
    /// resolved nothing for any other.
    ///
    /// Stale (TTL-lapsed) entries are filtered out rather than returned, so a
    /// closed tab whose entry hasn't been reclaimed yet can never be credited
    /// with a live session's permission prompt. H1 fix: tabs whose binding is
    /// ambiguous are filtered out too — see [`live_tab_sessions`].
    pub fn live_sessions_for(&self, harness: crate::harness::HarnessId) -> Vec<(String, String)> {
        let Some(agent) = harness.id() else {
            return Vec::new();
        };
        let now = crate::activity::now_ms() as i64;
        let Ok(live) = self.live_sessions.lock() else {
            return Vec::new();
        };
        let Ok(roots) = self.live_tab_roots.lock() else {
            return Vec::new();
        };
        live_tab_sessions(&live, &roots, agent, now)
    }

    /// V28 (issue #13): the session id the tab keyed `tab` currently reports,
    /// for `agent` (`"claude"` / `"opencode"`) — the read-side identity the
    /// `context_*` memory tools scope to, so two same-agent tabs on one project
    /// stop sharing a memory scope.
    ///
    /// This is the per-tab twin of [`Self::live_sessions_for`] (which stays —
    /// the permission-edge resolver needs a harness's whole mapping, not one
    /// tab). `None` means "no proof": no entry under that key, an entry left
    /// by a different agent, or a TTL-stale one. Every caller fails OPEN on
    /// `None` — back to `mem_current_session_for(agent)` — so a missing/unknown/
    /// stale tab can never error a tool call. H1 fix: an AMBIGUOUS tab (two
    /// running same-agent tabs on one transcript root) is `None` as well — same
    /// fail-open, see [`tab_binding_is_ambiguous`].
    pub fn live_session_for_tab(&self, tab: &str, agent: &str) -> Option<String> {
        let now = crate::activity::now_ms() as i64;
        let live = self.live_sessions.lock().ok()?;
        let roots = self.live_tab_roots.lock().ok()?;
        lookup_live_session_for_tab(&live, &roots, tab, agent, now)
    }

    /// V34: the session `tab` currently reports, whatever agent it runs.
    ///
    /// [`Self::live_session_for_tab`] is called by the MCP path, which already
    /// knows which agent it is serving and passes it as the check. A UI asking
    /// "what is the focused tab working on?" does not, and should not have to
    /// re-derive it from settings — the registry entry already names its own
    /// agent, so this reads it and applies the identical proof rules (exact key,
    /// TTL, agent match, ambiguity). `None` carries the same meaning as there:
    /// no proof, so callers fall back rather than guess.
    pub fn live_session_for_any_agent(&self, tab: &str) -> Option<String> {
        let now = crate::activity::now_ms() as i64;
        let live = self.live_sessions.lock().ok()?;
        let roots = self.live_tab_roots.lock().ok()?;
        let agent = live.get(&LiveKey::tab(tab))?.agent.clone();
        lookup_live_session_for_tab(&live, &roots, tab, &agent, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── V24 Phase B: live-session registry → active_session_ids ─────────────

    /// A minimal session row carrying just the id + `last_ms` the active-set
    /// logic reads (the rest is irrelevant to the decision).
    fn urow(id: &str, last_ms: i64) -> SessionUsageRow {
        SessionUsageRow {
            session_id: id.to_string(),
            agent: "claude".to_string(),
            totals: crate::harness::plugin::TokenKinds::default(),
            tool_chars: 0,
            cache_hit_ratio: None,
            est_only: false,
            started_ms: 0,
            last_ms,
            models: Vec::new(),
        }
    }

    fn live(session_id: &str, last_seen_ms: i64) -> LiveSession {
        LiveSession {
            agent: "claude".to_string(),
            session_id: session_id.to_string(),
            last_seen_ms,
        }
    }

    #[test]
    fn active_session_ids_unions_registry_and_recency_and_dedups() {
        let now = 10_000_000i64;
        let sessions = vec![
            urow("recent", now - 1_000), // recency-fresh
            urow("idle-but-open", now - LIVE_SESSION_RECENCY_MS - 60_000), // stale activity
            urow("stale", now - LIVE_SESSION_RECENCY_MS - 60_000), // stale, no live entry
        ];
        let mut reg = HashMap::new();
        // A still-ticking tab whose last activity fell out of the recency window
        // — the registry keeps it active (the point of the union).
        reg.insert(LiveKey::tab("tabA"), live("idle-but-open", now - 1_000));
        // An expired registry entry does NOT keep its session active.
        reg.insert(LiveKey::tab("tabB"),
            live("stale", now - LIVE_SESSION_TTL_MS - 1_000),
        );
        // A fresh entry whose session isn't in THIS root's list is ignored
        // (the registry is process-wide; the output is root-scoped).
        reg.insert(LiveKey::tab("tabC"), live("other-project", now));
        // "recent" is BOTH recency-fresh and registry-fresh → appears once.
        reg.insert(LiveKey::tab("tabD"), live("recent", now));

        let active = compute_active_session_ids(&reg, &sessions, now);
        assert_eq!(
            active,
            vec!["idle-but-open".to_string(), "recent".to_string()],
            "sorted, deduped, TTL-gated and root-scoped"
        );
    }

    #[test]
    fn active_session_ids_registry_ttl_boundary() {
        let now = 10_000_000i64;
        // A single session with stale activity, so only the registry can mark it.
        let sessions = vec![urow("s", now - LIVE_SESSION_RECENCY_MS - 1)];
        // Exactly at the TTL edge is still live...
        let mut at_edge = HashMap::new();
        at_edge.insert(LiveKey::tab("t"), live("s", now - LIVE_SESSION_TTL_MS));
        assert_eq!(
            compute_active_session_ids(&at_edge, &sessions, now),
            vec!["s".to_string()]
        );
        // ...one ms past it has expired.
        let mut past = HashMap::new();
        past.insert(LiveKey::tab("t"), live("s", now - LIVE_SESSION_TTL_MS - 1));
        assert!(compute_active_session_ids(&past, &sessions, now).is_empty());
    }

    // ── V28 (issue #13): tab → session resolution ─────────────────────────

    /// A registry entry for an arbitrary agent (the `live` helper pins Claude).
    fn live_for(agent: &str, session_id: &str, last_seen_ms: i64) -> LiveSession {
        LiveSession {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            last_seen_ms,
        }
    }

    fn v28_registry(now: i64) -> LiveRegistry {
        let mut reg = HashMap::new();
        reg.insert(LiveKey::tab("claude"),
            live_for("claude", "ses_a", now - 1_000),
        );
        reg.insert(LiveKey::tab("claude-local"),
            live_for("claude", "ses_b", now - 1_000),
        );
        reg.insert(LiveKey::tab("opencode"),
            live_for("opencode", "ses_oc", now - 1_000),
        );
        reg.insert(LiveKey::tab("claude-stale"),
            live_for("claude", "ses_old", now - LIVE_SESSION_TTL_MS - 1),
        );
        reg
    }

    /// No running-tab roots registered: every V28 lookup behaves exactly as it
    /// did before the H1 fix (the pre-existing tests all use this).
    fn no_roots() -> HashMap<String, LiveTabRoot> {
        HashMap::new()
    }

    #[test]
    fn live_session_for_tab_returns_that_tabs_own_session() {
        // The whole point of V28: two tabs of the SAME agent resolve to their
        // OWN sessions, not to whichever was most recently active.
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude", "claude", now),
            Some("ses_a".to_string())
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude-local", "claude", now),
            Some("ses_b".to_string())
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "opencode", "opencode", now),
            Some("ses_oc".to_string())
        );
    }

    #[test]
    fn live_session_for_tab_rejects_an_agent_mismatch() {
        // The key exists but was stamped by the other harness — resolving it
        // would hand a Claude call an OpenCode session. Fail open instead.
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "opencode", "claude", now),
            None
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude", "opencode", now),
            None
        );
    }

    #[test]
    fn live_session_for_tab_rejects_a_ttl_stale_entry() {
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude-stale", "claude", now),
            None,
            "past the TTL the tab's reported session is no longer proof"
        );
        // Exactly at the TTL edge still counts (same boundary as the registry
        // half of `compute_active_session_ids` and the eviction sweep).
        let mut edge = HashMap::new();
        edge.insert(LiveKey::tab("claude"),
            live_for("claude", "ses_edge", now - LIVE_SESSION_TTL_MS),
        );
        assert_eq!(
            lookup_live_session_for_tab(&edge, &no_roots(), "claude", "claude", now),
            Some("ses_edge".to_string())
        );
    }

    #[test]
    fn live_session_for_tab_never_guesses_on_an_unknown_key() {
        // No prefix/fuzzy matching, no "only one Claude entry, must be it".
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        for tab in ["", "claude2", "clau", "claude ", "CLAUDE"] {
            assert_eq!(
                lookup_live_session_for_tab(&reg, &no_roots(), tab, "claude", now),
                None,
                "tab key {tab:?} must not resolve"
            );
        }
        assert!(
            lookup_live_session_for_tab(&LiveRegistry::new(), &no_roots(), "claude", "claude", now)
                .is_none()
        );
    }

    // ── H1 (2026-08-05 review): same-root ambiguity degrades to unscoped ──

    /// An UNPINNED tab claim — the newest-wins binding these H1 cases are all
    /// about. V34's pinned tabs are covered separately below.
    fn root_at(agent: &str, root: &str, last_seen_ms: i64) -> LiveTabRoot {
        LiveTabRoot {
            agent: agent.to_string(),
            root: PathBuf::from(root),
            last_seen_ms,
            pinned: false,
        }
    }

    /// The pre-V34 mark: no `--session-id` pin, so the tab binds by tailing
    /// whichever transcript under `root` is newest.
    fn mark_unpinned(
        roots: &mut HashMap<String, LiveTabRoot>,
        tab: &str,
        agent: &str,
        root: &Path,
        now: i64,
    ) {
        upsert_live_tab_root(roots, tab, agent, root, false, now);
    }

    // ── V34: a pinned tab is provable regardless of co-tenants ──────────

    /// Two Claude tabs on one project — the exact configuration V28 decision
    /// 4a had to degrade — but tab A carries a `--session-id` pin. A pinned tap
    /// never consults "newest", so a co-tenant cannot make its binding wrong,
    /// and it must NOT be reported ambiguous.
    #[test]
    fn a_pinned_tab_is_never_ambiguous_however_many_co_tenants_share_its_root() {
        let now = 10_000_000i64;
        let shared = Path::new("/home/u/.claude/projects/P--proj");
        let mut reg = HashMap::new();
        upsert_live_tab_root(&mut reg, "claude", "claude", shared, true, now);
        mark_unpinned(&mut reg, "claude-local", "claude", shared, now);

        assert!(!tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // ...and the pin is what makes its session resolvable again.
        let live = LiveRegistry::from([(
            LiveKey::tab("claude"),
            LiveSession {
                agent: "claude".to_string(),
                session_id: "ses_pinned".to_string(),
                last_seen_ms: now,
            },
        )]);
        assert_eq!(
            lookup_live_session_for_tab(&live, &reg, "claude", "claude", now),
            Some("ses_pinned".to_string()),
        );
    }

    /// The asymmetry is deliberate: pinning tab A does not rescue tab B. B is
    /// still the one guessing, and the file it guesses at can just as easily be
    /// A's transcript as another guesser's.
    #[test]
    fn an_unpinned_tab_stays_ambiguous_beside_a_pinned_co_tenant() {
        let now = 10_000_000i64;
        let shared = Path::new("/home/u/.claude/projects/P--proj");
        let mut reg = HashMap::new();
        upsert_live_tab_root(&mut reg, "claude", "claude", shared, true, now);
        mark_unpinned(&mut reg, "claude-local", "claude", shared, now);

        assert!(tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
    }

    /// The pin is re-asserted on every mark, not latched, so a tap that gives
    /// up on its pin (the harness never wrote the pinned transcript) degrades
    /// the tab back to ambiguous rather than keeping a proof it no longer has.
    #[test]
    fn dropping_the_pin_restores_ambiguity() {
        let now = 10_000_000i64;
        let shared = Path::new("/home/u/.claude/projects/P--proj");
        let mut reg = HashMap::new();
        upsert_live_tab_root(&mut reg, "claude", "claude", shared, true, now);
        mark_unpinned(&mut reg, "claude-local", "claude", shared, now);
        assert!(!tab_binding_is_ambiguous(&reg, "claude", "claude", now));

        // The tap's next tick, after `pin_step` returned `GiveUp`.
        mark_unpinned(&mut reg, "claude", "claude", shared, now);
        assert!(tab_binding_is_ambiguous(&reg, "claude", "claude", now));
    }

    /// `n` running Claude tabs, all tailing the SAME transcript root.
    fn roots_sharing(tabs: &[&str], now: i64) -> HashMap<String, LiveTabRoot> {
        tabs.iter()
            .map(|t| {
                (
                    (*t).to_string(),
                    root_at("claude", "/home/u/.claude/projects/P--proj", now - 100),
                )
            })
            .collect()
    }

    #[test]
    fn ambiguity_predicate_counts_running_tabs_sharing_a_root() {
        let now = 10_000_000i64;
        // 0 running tabs registered → nothing to conflate.
        assert!(!tab_binding_is_ambiguous(&no_roots(), "claude", "claude", now));
        // 1 running tab on the root → the common case, NOT ambiguous.
        let one = roots_sharing(&["claude"], now);
        assert!(!tab_binding_is_ambiguous(&one, "claude", "claude", now));
        // 2 running tabs on the SAME root → both are ambiguous.
        let two = roots_sharing(&["claude", "claude-local"], now);
        assert!(tab_binding_is_ambiguous(&two, "claude", "claude", now));
        assert!(tab_binding_is_ambiguous(&two, "claude-local", "claude", now));
        // 2 running tabs on DIFFERENT roots → each keeps its own identity.
        let mut split = HashMap::new();
        split.insert("claude".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--one", now),
        );
        split.insert("claude-local".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--two", now),
        );
        assert!(!tab_binding_is_ambiguous(&split, "claude", "claude", now));
        assert!(!tab_binding_is_ambiguous(
            &split,
            "claude-local",
            "claude",
            now
        ));
    }

    #[test]
    fn ambiguity_predicate_ignores_other_agents_and_stale_co_tenants() {
        let now = 10_000_000i64;
        let shared = "/home/u/.claude/projects/P--proj";
        let mut reg = HashMap::new();
        reg.insert("claude".to_string(), root_at("claude", shared, now));
        // A different agent on the same root is not a co-tenant: OpenCode binds
        // per-tab off its own stream (and never registers here anyway).
        reg.insert("opencode".to_string(), root_at("opencode", shared, now));
        assert!(!tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // A CLOSED tab whose entry outlived the TTL is not a co-tenant either —
        // otherwise a leaked entry would disable scoping forever.
        reg.insert("claude-local".to_string(),
            root_at("claude", shared, now - LIVE_SESSION_TTL_MS - 1),
        );
        assert!(!tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // Refresh it (the tab is running again) and ambiguity returns.
        reg.insert("claude-local".to_string(), root_at("claude", shared, now));
        assert!(tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // A tab with no root entry at all is never degraded (OpenCode's path).
        assert!(!tab_binding_is_ambiguous(&reg, "opencode-2", "opencode", now));
    }

    #[test]
    fn live_session_for_tab_is_unscoped_under_same_root_ambiguity() {
        // The H1 case: two Claude tabs on one project. The registry answers
        // "whichever session wrote last" for BOTH keys, so honoring it would
        // put tab A's memory notes in tab B's scope. Fail open to unscoped.
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        let two = roots_sharing(&["claude", "claude-local"], now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &two, "claude", "claude", now),
            None
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &two, "claude-local", "claude", now),
            None
        );
        // The single-running-tab case is untouched.
        let one = roots_sharing(&["claude"], now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &one, "claude", "claude", now),
            Some("ses_a".to_string())
        );
        // ...and an OpenCode tab never registers a root, so it never degrades.
        assert_eq!(
            lookup_live_session_for_tab(&reg, &two, "opencode", "opencode", now),
            Some("ses_oc".to_string())
        );
    }

    #[test]
    fn live_tab_sessions_drops_ambiguous_tabs_only() {
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        // Single running tab per root: the permission resolver still gets the
        // mapping it needs (TTL-stale entries filtered as before).
        let one = roots_sharing(&["claude"], now);
        let mut got = live_tab_sessions(&reg, &one, "claude", now);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("claude".to_string(), "ses_a".to_string()),
                ("claude-local".to_string(), "ses_b".to_string()),
            ],
            "only `claude` is registered as running, so nothing is ambiguous"
        );
        // Both tabs running on one root — including the launch-order window in
        // which A's tap already rotated onto B's fresh session and marked it
        // live UNIQUELY: no pair survives, so the resolver has nothing to
        // attribute with and refuses.
        let two = roots_sharing(&["claude", "claude-local"], now);
        assert!(live_tab_sessions(&reg, &two, "claude", now).is_empty());
        let mut window = HashMap::new();
        window.insert(LiveKey::tab("claude"),
            live_for("claude", "ses_b", now - 10), // A's tap, B's session
        );
        assert!(live_tab_sessions(&window, &two, "claude", now).is_empty());
        // Two tabs on DIFFERENT roots keep their pairs.
        let mut split = HashMap::new();
        split.insert("claude".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--one", now),
        );
        split.insert("claude-local".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--two", now),
        );
        assert_eq!(live_tab_sessions(&reg, &split, "claude", now).len(), 2);
    }

    /// H1-R5: the root is a normalized comparison key, so two tabs whose cwds
    /// were typed with different separators/trailing slashes (and, on Windows,
    /// different case) are still recognized as co-tenants. Before the fix these
    /// produced different `PathBuf` keys and the predicate silently answered
    /// "not ambiguous" — i.e. confident-wrong scoping for both tabs.
    #[test]
    fn tab_root_keys_are_normalized_at_the_mark_site() {
        let now = 10_000_000i64;
        let mut reg = HashMap::new();
        mark_unpinned(
            &mut reg,
            "claude",
            "claude",
            Path::new(r"C:\Users\u\.claude\projects\P--proj"),
            now,
        );
        mark_unpinned(
            &mut reg,
            "claude-local",
            "claude",
            Path::new("C:/Users/u/.claude/projects/P--proj/"),
            now,
        );
        assert!(
            tab_binding_is_ambiguous(&reg, "claude", "claude", now),
            "separator/trailing-slash variants of one dir must conflate"
        );
        assert!(tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
        // Windows paths are case-insensitive, so a case variant is the SAME dir.
        if cfg!(windows) {
            let mut cased = HashMap::new();
            mark_unpinned(
                &mut cased,
                "claude",
                "claude",
                Path::new(r"C:\Users\u\.claude\projects\P--Proj"),
                now,
            );
            mark_unpinned(
                &mut cased,
                "claude-local",
                "claude",
                Path::new(r"c:\users\u\.claude\projects\p--proj"),
                now,
            );
            assert!(
                tab_binding_is_ambiguous(&cased, "claude", "claude", now),
                "case variants of one Windows dir must conflate"
            );
        }
        // Genuinely different dirs still don't conflate.
        let mut split = HashMap::new();
        mark_unpinned(&mut split, "claude", "claude", Path::new("/u/p/one"), now);
        mark_unpinned(&mut split, "claude-local", "claude", Path::new("/u/p/two"), now);
        assert!(!tab_binding_is_ambiguous(&split, "claude", "claude", now));
    }

    /// H1-R2: the property the tap's heartbeat depends on — a refresh restores a
    /// claim that had aged past the TTL, so a tab stalled inside TTS
    /// backpressure keeps counting as a co-tenant instead of letting its sibling
    /// become "unique" (and confidently bind to the stalled tab's transcript).
    #[test]
    fn refreshing_a_tab_root_restores_a_ttl_stale_claim() {
        let now = 10_000_000i64;
        let shared = Path::new("/home/u/.claude/projects/P--proj");
        let mut reg = HashMap::new();
        // Tab A marked long ago (its drain loop is parked in `speak`), tab B
        // ticking normally.
        mark_unpinned(
            &mut reg,
            "claude",
            "claude",
            shared,
            now - LIVE_SESSION_TTL_MS - 1,
        );
        mark_unpinned(&mut reg, "claude-local", "claude", shared, now);
        // The starvation symptom: A aged out, so B looks unique and would get a
        // confident (wrong) binding.
        assert!(!reg.contains_key("claude"), "stale claim is evicted");
        assert!(!tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
        // A heartbeat tick re-marks A — independent of A's drain loop — and the
        // co-tenancy is visible again for BOTH tabs.
        mark_unpinned(&mut reg, "claude", "claude", shared, now);
        assert!(tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        assert!(tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
    }

    /// **The C-2 collision, made unrepresentable** (V40 Phase D, locked
    /// decision 20).
    ///
    /// A `/memory/event` writes into the SESSION space. Even when the id it
    /// carries is character-for-character a configured tab id, every tab-keyed
    /// reader — the memory-scoping lookup, the permission-edge mapping, the
    /// "what is this tab working on" query — looks in the TAB space and cannot
    /// see it. That is what replaced the predicate that used to refuse such a
    /// key, and this is the assertion that the replacement actually holds.
    #[test]
    fn a_session_space_entry_is_invisible_to_every_tab_keyed_reader() {
        let now = 10_000_000i64;
        let mut reg = LiveRegistry::new();
        // The tab's own reader: tab space, the true session.
        reg.insert(LiveKey::tab("claude"), live_for("claude", "ses_true", now));
        // A forged POST naming that tab id: session space.
        reg.insert(
            LiveKey::session("claude"),
            live_for("opencode", "ses_forged", now),
        );

        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude", "claude", now).as_deref(),
            Some("ses_true"),
            "the tab-keyed reader must still see its own reader's binding"
        );
        assert_eq!(
            live_tab_sessions(&reg, &no_roots(), "claude", now),
            vec![("claude".to_string(), "ses_true".to_string())],
            "the permission-edge mapping must not pick up the session-space row"
        );
        assert!(
            live_tab_sessions(&reg, &no_roots(), "opencode", now).is_empty(),
            "…and must not report the forged row under the agent that sent it either"
        );
    }

    /// `live_sessions_for`'s pure half answers PER HARNESS, which is the half
    /// that used to be a `"claude"` literal — an OpenCode tab's live session was
    /// invisible to the permission-edge resolver however fresh it was.
    #[test]
    fn live_tab_sessions_answers_for_each_harness() {
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        let claude: Vec<String> = live_tab_sessions(&reg, &no_roots(), "claude", now)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(claude.contains(&"claude".to_string()));
        assert!(!claude.contains(&"opencode".to_string()));
        assert_eq!(
            live_tab_sessions(&reg, &no_roots(), "opencode", now),
            vec![("opencode".to_string(), "ses_oc".to_string())],
            "an OpenCode tab's live session is now resolvable, not silently empty"
        );
        assert!(
            live_tab_sessions(&reg, &no_roots(), "not-a-harness", now).is_empty(),
            "an unknown agent resolves nothing"
        );
    }

    #[test]
    fn evict_stale_live_sessions_drops_only_ttl_stale_entries() {
        // V24 code-review: the opportunistic eviction `mark_live_session` runs
        // keeps entries within the registry TTL (which the registry half still
        // uses) and drops only those past it, so OpenCode keys can't accumulate.
        let now = 10_000_000i64;
        let mut reg = HashMap::new();
        reg.insert(LiveKey::tab("fresh"), live("s_fresh", now - 1_000));
        reg.insert(LiveKey::tab("edge"),
            live("s_edge", now - LIVE_SESSION_TTL_MS),
        );
        reg.insert(LiveKey::tab("stale"),
            live("s_stale", now - LIVE_SESSION_TTL_MS - 1),
        );
        evict_stale_live_sessions(&mut reg, now);
        assert!(reg.contains_key(&LiveKey::tab("fresh")), "within TTL kept");
        assert!(reg.contains_key(&LiveKey::tab("edge")), "exactly at TTL kept");
        assert!(!reg.contains_key(&LiveKey::tab("stale")), "past TTL evicted");
        assert_eq!(reg.len(), 2);
    }
}

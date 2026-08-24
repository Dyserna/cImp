//! V32 Phase B — the consumer-side taint latch.
//!
//! The containment subsystem the offload proxy runs in front of every gated
//! call: which tab is calling, whether its latch is engaged, what a write it
//! makes is worth, and what the user is allowed to do about it.
//!
//! It lived inside `offload/loopback.rs` until V42 R3 (#114) — ~3 000 lines,
//! 31 % of that file's production code — while almost every consumer is
//! OUTSIDE it: `offload::toolclass`, `offload::outbound`, `offload::agent`,
//! `ipc::commands` and `harness::claude::hook`. It moved here verbatim: no
//! behaviour, no wire bytes and no user-visible string changed, and the only
//! edits are visibility (`pub(super)` where loopback's handlers or its tests
//! were the caller — `pub(super)` here means `offload`, and nothing wider).
//!
//! **It is deliberately ONE module.** The obvious internal seams — identity,
//! per-tab state, the registry, the Events rows — are not module boundaries:
//! [`LatchRegistry::gate`] and the row builders read [`LatchScope`]'s and
//! `TabLatch`'s private fields directly, so splitting the file would mean
//! opening ~45 fields and methods of a containment state machine to a sibling
//! module. That is an encapsulation change, not code motion, and it does not
//! belong in a behaviour-preserving commit; the field-privacy boundary here is
//! load-bearing (it is why `contaminated` cannot be cleared except through
//! [`LatchRegistry::apply_override`]). Accessors first, then a split, if a
//! split is ever wanted.
//!
//! The reading order inside the file is unchanged: identity
//! ([`LatchScope`], [`latch_scope`]) → one tab's state (`TabLatch`) → the
//! process-wide registry and its gate ([`latches`]) → the audit rows a state
//! change owes → [`apply_latch_override`], the one user-initiated entry point
//! (the badge popover, over IPC — never over HTTP).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use serde::Serialize;
use super::host::RouteCtx;
use tracing::{info, warn};

// V42 review (dropped-at-cap): these two used to come from `super::loopback` —
// a back-edge from the module V42 R3 (#114) extracted to the module it was
// extracted from, and the reason this file could not be read as sitting below
// the router. Both live in `offload` itself now; neither was ever about
// routing.
use super::bounded_id;
use super::outbound::{self, Budget};
use super::toolclass::{self, Latch, ProxyGate, ToolClass, WriteTaint};

/// The identity one gated call carries: which agent, which tab, and which
/// session that tab is currently running.
///
/// `agent` is always the normalized `claude`/`opencode` vocabulary
/// ([`crate::graph::source_for_consumer`]) because the two gated routes learn
/// the consumer differently — `/graph_run` from the body, `/mcp/call` from the
/// `?consumer=` query — and one tab MUST key identically from either, or its
/// web fetches and its graph reads would latch two separate scopes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LatchScope {
    pub(super) agent: &'static str,
    pub(super) tab: String,
    /// The session the V28 registry currently reports for this tab, or `None`
    /// when it withholds one (no live entry yet, TTL-stale, or the H1
    /// same-root ambiguity). `None` is *absence of evidence*, never evidence of
    /// a new session — see [`TabLatch::observe`].
    pub(super) session: Option<String>,
    /// The project root this tab runs against, in [`crate::activity::root_key`]
    /// form — the `root` column of the activity rows this scope's calls
    /// produce (#48, finding F-3).
    ///
    /// It rides on the scope rather than on each call because it is a property
    /// of the tab, and because [`latch_scope`] is the ONE funnel every gated
    /// route resolves identity through: a row written from a path that has a
    /// scope therefore cannot be written without a root, which is the mistake
    /// `beacon_row` and the memory-quarantine row made (`root: ""`, so neither
    /// can be filtered per project, so neither can appear on a per-project
    /// surface).
    ///
    /// **Resolved from settings, not from the request.** The gated bodies all
    /// carry a `cwd`, but that is the calling child's claim about itself; the
    /// tab id is config-derived and validated against the same snapshot
    /// ([`is_configured_tab`]), so `crate::tabs::ai_tab_dir` gives a root with
    /// the same trust level as the identity it hangs off. Empty only if no
    /// working directory can be resolved at all — see [`tab_root_key`].
    pub(super) root: String,
}

impl LatchScope {
    /// The registry key. Tuple, not a formatted string, so no tab id
    /// containing the separator could collide with another agent's tab.
    fn key(&self) -> (&'static str, String) {
        (self.agent, self.tab.clone())
    }

    /// V32 Phase G: this scope as the injection resolver addresses it. The two
    /// vocabularies are deliberately the same pair (`agent`, `tab`) so a tab's
    /// latch and a tab's override row can never key differently.
    pub(super) fn injection(&self) -> crate::settings::injection::Scope<'_> {
        crate::settings::injection::Scope::Tab {
            agent: self.agent,
            tab: &self.tab,
        }
    }

    /// The human-readable scope label carried by V32 `injection_flag` activity
    /// rows. Formatted (unlike [`key`](Self::key)) because it is for a reader,
    /// not for equality.
    pub(super) fn label(&self) -> String {
        format!("{}:{}", self.agent, self.tab)
    }

    /// This scope as an activity-row attribution (#48 F-29).
    ///
    /// A scope exists only for a **configured** tab id ([`is_configured_tab`],
    /// checked by [`latch_scope`]), so [`Attribution::Tab`] is a fact here — the
    /// same reading [`LatchScoping::attribution`] takes for its `Scoped` arm,
    /// which now delegates here so the two cannot drift.
    ///
    /// The tab id is taken from the field, never re-split out of
    /// [`label`](Self::label): a round trip through a formatted string is how
    /// `"{agent}:(no tab identity)"` became a *tab named `(no tab identity)`*
    /// once already (`outbound::scope_attribution`'s doc).
    pub(super) fn attribution(&self) -> crate::activity::Attribution {
        crate::activity::Attribution::Tab(self.tab.clone())
    }
}

/// Whether `tab` names an AI tab the **user has configured for `agent`** (#45;
/// consumer-scoped by V33 C5, finding F-4).
///
/// This is the predicate that makes [`latches`]' "bounded by construction"
/// claim true rather than aspirational. Every registry entry is keyed on a
/// tab id that arrives in a request body, so without this the map's key space
/// is "whatever a caller typed" — no TTL, no cap, no eviction, and every entry
/// serialized into every `/status` response and every 4 s `latch_status` poll.
/// With it, the key space is a subset of the user's own tab list.
///
/// **V33 C5 — the pair, not the halves.** Until V33 this asked only "is this
/// *some* configured AI tab id", while every registry key is the PAIR
/// `(agent, tab)` ([`LatchScope::key`]) and `agent` is caller-asserted on every
/// route that has one. A caller could therefore key a latch under
/// `("claude", <an OpenCode tab's id>)` and the pair was verified on no route in
/// the system. It is now verified here, at the one funnel
/// ([`latch_scope`]) every entry-creating path resolves through: the id must
/// name a configured tab **of the asserted consumer**, classified by
/// [`crate::tabs::tab_consumer`] — the same call the launch path makes when it
/// decides what to inject into that tab, so the two ends cannot drift.
///
/// **Not a live exploit today, a restored invariant.** The V32 review rated the
/// cross-keyed case harmless on the routes that exist: a latch keyed under the
/// wrong agent is freshly open, engages a scope nobody reads, and refuses
/// nothing. What it bought was a registry key space twice the size of the tab
/// list and a `(consumer, tab)` pair no route checked.
///
/// **The check is still "is this a configured tab id of this consumer", NOT "is
/// this the tab that owns this connection".** The stricter form would break
/// legitimate beacons today: the OpenCode plugin file is written per *directory*
/// (one file per tab since #48's H-2 fix, but every tab in a directory still
/// loads every file), so the tab id baked into it may belong to a different tab
/// sharing the same working dir. Whoever fixes H-2's remainder may tighten this;
/// until then, binding a beacon to its connection would reject real beacons from
/// real tabs.
///
/// **`AiTool` tabs only.** Shell and Preview tabs host no harness, so nothing
/// legitimate can beacon or gate as one.
///
/// **The empty-list escape**, and why it is keyed on the WHOLE list rather than
/// on this consumer's slice. With no AI tab configured at all the predicate
/// accepts everything, because [`RouteCtx::settings`] falls back to
/// `Settings::default()` (whose `tabs` is empty) when managed state is not up
/// yet — and a request arriving in that window must not be rejected on the
/// strength of a list we could not read. That condition is "settings are
/// unreadable", which is global; narrowing the *floor* to "this consumer has no
/// tabs" would have widened it instead, handing every forged id a scope on any
/// install that runs only Claude tabs or only OpenCode ones — i.e. re-opening
/// exactly the unbounded key space #45 closed. So the floor keeps its original
/// trigger and only the positive test is consumer-scoped, which makes this
/// change a strict tightening of the admitted set.
pub(crate) fn is_configured_tab(settings: &crate::settings::Settings, agent: &'static str, tab: &str) -> bool {
    names_a_configured_ai_tab_for(settings, agent, tab) || ai_tab_ids(settings).next().is_none()
}

/// Every configured AI tab's id, in settings order — **every consumer's**.
///
/// One caller, and it is not the latch: [`is_configured_tab`]'s availability
/// floor, whose condition is "settings are unreadable", not "this consumer has
/// no tabs". Identity checks use [`ai_tab_ids_for`] instead. (The second
/// caller, the C-2 collision check, went with V40 Phase D's key spaces.)
pub(super) fn ai_tab_ids(settings: &crate::settings::Settings) -> impl Iterator<Item = &str> {
    settings.tabs.iter().filter_map(|t| match t {
        crate::settings::TabConfig::AiTool(c) => Some(c.id.as_str()),
        _ => None,
    })
}

/// Every configured AI tab id belonging to `agent` (`"claude"` / `"opencode"`),
/// in settings order — V33 C5's key space.
pub(super) fn ai_tab_ids_for<'a>(
    settings: &'a crate::settings::Settings,
    agent: &'static str,
) -> impl Iterator<Item = &'a str> {
    settings.tabs.iter().filter_map(move |t| match t {
        crate::settings::TabConfig::AiTool(c) if crate::tabs::tab_consumer(c) == Some(agent) => {
            Some(c.id.as_str())
        }
        _ => None,
    })
}

/// Whether `id` exactly names a configured AI tab **of `agent`** —
/// [`is_configured_tab`] without its availability floor.
pub(super) fn names_a_configured_ai_tab_for(
    settings: &crate::settings::Settings,
    agent: &'static str,
    id: &str,
) -> bool {
    ai_tab_ids_for(settings, agent).any(|t| t == id)
}

// `names_a_configured_ai_tab` lived here: "does this session id collide with a
// configured TAB id", the C-2 guard on `/memory/event`'s registry writes. V40
// Phase D deleted it with the collision it guarded — the live-session registry
// has two key spaces now (locked decision 20), a body-supplied id goes into the
// session space, and no string can be in both. See
// `mark_live_session_from_body`.

/// Which of three cases a request body's `(agent, tab)` falls into, decided
/// **without** the `AppHandle` [`latch_scope`]'s session lookup needs.
///
/// V33 C5: `agent` is part of the question, not context carried alongside it —
/// `Configured` now means "a tab of THIS consumer", which is what the registry
/// key `(agent, tab)` has always asserted and nothing checked.
///
/// Split out (#48) for two reasons. It is the enforcement point for the
/// registry bound, and a bound asserted by calling [`is_configured_tab`] beside
/// `latch_scope` rather than through it survives deleting the call from
/// `latch_scope` — which is what
/// `tests::only_configured_ai_tab_ids_can_ever_key_a_latch` did. And it names
/// the distinction #45 collapsed: "no tab id" and "an id that names no
/// configured tab" are the same for the *registry* and not the same for a
/// *verdict*.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TabIdentity<'a> {
    /// No `tab` at all (absent, empty, or whitespace) — a child spawned before
    /// `--tab` existed.
    Anonymous,
    /// A non-empty id naming no configured AI tab. The trimmed id is carried
    /// for the log lines and error messages that have to quote it back.
    Unknown(&'a str),
    /// A configured AI tab id, trimmed.
    Configured(&'a str),
}

pub(super) fn tab_identity<'a>(
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&'a str>,
) -> TabIdentity<'a> {
    let Some(tab) = tab.map(str::trim).filter(|t| !t.is_empty()) else {
        return TabIdentity::Anonymous;
    };
    if is_configured_tab(settings, agent, tab) {
        TabIdentity::Configured(tab)
    } else {
        TabIdentity::Unknown(tab)
    }
}

/// Resolve the calling tab's latch scope, keeping the two identity-less cases
/// apart (#48).
///
/// [`LatchScoping::scope`] is `None` for both, which is the **fail-open** case
/// (locked, and V28's existing discipline): a child spawned before `--tab`
/// existed sends nothing, and a tool call must never fail for lack of identity.
/// It is deliberately NOT promoted to a global latch — one identityless call
/// would then latch every consumer at once. Such calls still get the
/// spotlighting envelope on EXTERNAL results, which needs no identity.
///
/// #45 widened "no identity" to include **an id that is not a configured tab**,
/// and V33 C5 widened it again to **an id that is not a configured tab of the
/// asserted consumer** ([`is_configured_tab`]). This is the single funnel every entry-creating path
/// resolves through — `/graph_run` and `/mcp/call` via `gate`, `/latch/beacon`
/// via `beacon` — so validating here is what bounds the registry, rather than
/// three route-local checks that can drift apart. An unknown id creates no row
/// and gates nothing; a caller that invents ids only ever talks to a scope that
/// does not exist, which is where it started. **That part is unchanged.**
///
/// What #48 changes is only that the two cases are now *distinguishable* by the
/// caller. Folding them into one `Option::None` also folded them into
/// `handle_latch_state`'s hard-off verdict, which was a Phase H regression: see
/// [`LatchScoping::Unknown`].
///
/// Takes the settings snapshot rather than reading its own, so a handler
/// resolves identity and policy under the SAME snapshot (the "ONE settings read
/// for the whole call" discipline `/mcp/call` documents).
pub(super) fn latch_scope(
    ctx: &RouteCtx,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
) -> LatchScoping {
    match tab_identity(settings, agent, tab) {
        TabIdentity::Anonymous => LatchScoping::Anonymous,
        TabIdentity::Unknown(tab) => LatchScoping::Unknown(tab.to_string()),
        TabIdentity::Configured(tab) => {
            let session = ctx
                .graph()
                .and_then(|g| g.live_session_for_tab(tab, agent));
            LatchScoping::Scoped(LatchScope {
                agent,
                tab: tab.to_string(),
                session,
                root: tab_root_key(ctx, settings, tab),
            })
        }
    }
}

/// The project root one configured AI tab runs against, as an activity
/// `root_key` (#48, finding F-3). See [`LatchScope::root`] for why the tab —
/// rather than the request body's `cwd` — is the source.
///
/// Two fallbacks, in order, and both are deliberate:
///
/// 1. **The app's launch directory**, when the id resolves to no AI tab config.
///    That is reachable through [`is_configured_tab`]'s empty-list escape (a
///    request that arrives before managed state is up), and the launch dir is
///    what such a tab *would* run in — [`crate::tabs::ai_tab_dir`] returns the
///    per-tab `cwd` override or exactly this.
/// 2. **The process cwd**, when managed state is not up at all. The app sets
///    `LaunchContext::cwd` from the process cwd at startup and never chdirs, so
///    this is the same directory by another route rather than a guess.
///
/// An empty string is possible only if even `current_dir()` fails (a deleted
/// cwd). It is not papered over with a placeholder: a root that cannot be
/// resolved must read as absent, not as some other project.
pub(super) fn tab_root_key(ctx: &RouteCtx, settings: &crate::settings::Settings, tab: &str) -> String {
    let launch = ctx
        .core()
        .map(|s| s.launch_cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let Some(launch) = launch else {
        return String::new();
    };
    let dir = crate::tabs::ai_tab_dir(settings, tab, &launch).unwrap_or(launch);
    crate::activity::root_key(&dir)
}

/// The outcome of [`latch_scope`] — [`TabIdentity`] with the session folded in.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LatchScoping {
    /// No tab identity at all. Fail-open everywhere.
    Anonymous,
    /// An id that names no configured AI tab — a forged one, or (the case that
    /// makes this worth a variant) a **stale real one**: the OpenCode plugin is
    /// written per working *directory* with one tab id baked in (the unfixed
    /// H-2), so removing or re-id'ing that tab leaves the file on disk still
    /// naming an id the settings no longer have.
    ///
    /// It keys no registry entry — that is #45's bound and it is untouched —
    /// but it must not be read as "containment is off" either, because the two
    /// look identical to the plugin and only one of them is a decision anyone
    /// took. See `handle_latch_state`.
    Unknown(String),
    /// A configured AI tab. The only variant that can key the registry.
    Scoped(LatchScope),
}

impl LatchScoping {
    /// The scope, when there is one. `None` for both identity-less variants —
    /// the fail-open reading `gate`, `beacon` and `budget_gate` take, and the
    /// reason an unknown id still creates no registry entry.
    pub(super) fn scope(&self) -> Option<&LatchScope> {
        match self {
            LatchScoping::Scoped(s) => Some(s),
            _ => None,
        }
    }

    /// Consume into the scope, for the callers that need to keep it.
    pub(super) fn into_scope(self) -> Option<LatchScope> {
        match self {
            LatchScoping::Scoped(s) => Some(s),
            _ => None,
        }
    }

    /// #51 / #48 F-20 — which tab an activity row written for this call belongs
    /// to.
    ///
    /// The mapping is one-to-one onto [`crate::activity::Attribution`], and that
    /// is the whole point: both enums were derived from the same three facts — no
    /// tab identity at all / an id naming no configured tab / a configured tab —
    /// and the row's column exists to report which of the three this call was.
    /// Written here, once, so `/graph_run` and `/mcp/call` cannot answer it
    /// differently, and so a future route gets the answer by resolving identity
    /// rather than by remembering to.
    ///
    /// Both handlers used to call [`Self::into_scope`] immediately, which
    /// collapses `Anonymous` and `Unknown` into one `None`. That collapse is
    /// right for the latch (both fail open) and wrong for the row, which has to
    /// keep them apart — that collapse IS finding F-20.
    ///
    /// **Not `Attribution::from_child_argv`.** A tab id that reached this frame
    /// came out of a request BODY, which a caller can invent; the argv
    /// constructor's own doc forbids it here. [`latch_scope`] has already run the
    /// id through [`is_configured_tab`], and `Unrecognized` is what the
    /// unvalidated case is called on the row.
    ///
    /// **#48 F-39 / locked decision 42 — that invented id is BOUNDED here.** It
    /// is an arbitrary-length string from a request body and it lands in a row's
    /// attribution column, in a **capped per-lane ring**: a caller choosing how
    /// many bytes one row occupies is choosing how much of the lane it fills, and
    /// the rows that fall out the other end are the genuine ones. Same
    /// consequence as F-37 by a different route, and the same cure F-32 already
    /// left in the tree — [`bounded_id`], **applied AFTER classification**, which
    /// is the load-bearing half: [`latch_scope`] resolved this variant by running
    /// the FULL string through [`is_configured_tab`], so no truncated invented id
    /// can ever fold onto a configured one. Bounding at the parse boundary
    /// instead would close a bloat hole by opening an impersonation hole.
    ///
    /// [`Attribution::Unattributed`](crate::activity::Attribution::Unattributed)
    /// is deliberately unreachable from this function: a route that resolved a
    /// `LatchScoping` at all DOES know, and "the writer does not know" would be a
    /// false claim — the one thing that column must never make.
    pub(super) fn attribution(&self) -> crate::activity::Attribution {
        match self {
            LatchScoping::Anonymous => crate::activity::Attribution::Headless,
            LatchScoping::Unknown(id) => {
                crate::activity::Attribution::Unrecognized(bounded_id(id))
            }
            LatchScoping::Scoped(s) => s.attribution(),
        }
    }

    /// The injection-hierarchy scope this call resolves features against.
    /// Both identity-less variants resolve as an **unknown caller**
    /// (`Scope::UnknownCaller`), the same fail-open reading
    /// [`GatePolicy::resolve`] has always taken for a scope-less call: the
    /// app-wide answer is the honest floor when there is no tab to ask about,
    /// and it is what an unrecognized id resolved to before #45 (L3 not found ⇒
    /// `Inherit` ⇒ L2 ⇒ L1).
    ///
    /// #48 F-35: that variant was called `Scope::App` until locked decision 36
    /// split it in two. Behaviour is unchanged — this site was always asking the
    /// identity-less question, and it keeps N-1's elevation (any configured
    /// tab's L3 `On` is honoured, because the caller IS one of those tabs).
    pub(super) fn injection(&self) -> crate::settings::injection::Scope<'_> {
        self.scope().map_or(
            crate::settings::injection::Scope::UnknownCaller,
            LatchScope::injection,
        )
    }
}

/// One tab's latch, together with the session identity it was engaged for.
pub(super) struct TabLatch {
    pub(super) session: Option<String>,
    pub(super) latch: Latch,
    /// V32 Phase C: this session's EXTERNAL call/byte spend (locked decision
    /// 11). It lives *here*, beside the latch, precisely so it inherits the
    /// latch's scope and reset rule — one conversation, one budget, both
    /// cleared together when the tab's session rotates. (H-2: `contaminated`
    /// no longer rides along; a permissive reset and an un-tainting reset need
    /// different evidence — see [`TabLatch::contaminated`].)
    pub(super) budget: Budget,
    /// Whether this session's taint-latch refusal has already been reported to
    /// the Tool Activity feed. One row per scope: the latch is sticky, so every
    /// later refusal restates the same fact.
    pub(super) latch_flagged: bool,
    /// Whether this session's native-web BEACON has already been reported
    /// (#48). Same one-row-per-scope bound and the same reset as
    /// [`latch_flagged`](Self::latch_flagged), and it exists for the same
    /// reason: a caller that POSTs `/latch/beacon` in a loop must produce one
    /// row, not one per request, or it floods a capped feed and evicts the rows
    /// the audit trail exists to keep.
    ///
    /// It is a separate bit from "the latch moved" because #45 keyed the row on
    /// the latch transition alone, and a beacon can change this conversation's
    /// state **without** moving the latch: a tab already latched `Local`
    /// (Phase A's other direction) takes the contamination bit and keeps its
    /// latch, which then quarantines every later `context_note` — silently,
    /// under the old condition.
    pub(super) beacon_flagged: bool,
    /// V32 Phase F (locked decision 15): whether external content has entered
    /// this conversation *at all* — set the moment an EXTERNAL call is admitted
    /// (proxied, or beaconed from a harness-native web tool) and cleared by
    /// exactly three USER actions, none of them reachable over HTTP (see "Step
    /// 4" and "The 2026-08-10 amendment" below).
    ///
    /// It exists because decision 15 lets the USER move the latch, and the two
    /// facts then come apart: the latch says what the session may do NEXT,
    /// while contamination says what is already in its context window. A note
    /// written after "switch to local" was still composed by a model that read
    /// an attacker's page, so persistence must stay quarantined — contamination
    /// is a property of the conversation, not of the latch position.
    ///
    /// That argument governs [`LatchOverride::FlipLocal`] and only it. The
    /// 2026-08-10 amendment below draws the line where the user's own click
    /// draws it: a *workflow step* cannot un-taint a conversation, but a
    /// deliberate return to FULL access — the strictly larger risk — is the
    /// verdict the flip is not.
    ///
    /// # H-2 (2026-08-08 re-review): the bit is STICKY, and why
    ///
    /// It used to be cleared by [`TabLatch::observe`] on a session rotation,
    /// "because only a new conversation has a clean context". The premise was
    /// wrong about *who decides* a rotation happened. A Claude tab's session id
    /// is the file stem of the newest `*.jsonl` under its transcript root, and
    /// **decision 3 puts Claude's native Bash outside every cImp latch** — so
    /// the model can create that file itself. Every bar the tap could put in
    /// front of the rotation (the file exists / the file grew / the file
    /// decodes as a transcript naming that session) is a bar over a file the
    /// attacker is writing. C-2 fixed "the file exists", H-2 defeated "the file
    /// grew" with one trailing newline, and "it decodes" falls to `echo
    /// '{"sessionId":"…"}'`. **No filesystem-derived fact can be the trust root
    /// for un-tainting a context window**, so the reset is gone rather than
    /// re-armed.
    ///
    /// This amends locked decision 15: contamination is now a property of the
    /// **tab**, not of the conversation, deliberately — because the conversation
    /// boundary is attacker-controlled and the tab id is not (it is
    /// config-derived, and [`is_configured_tab`] bounds the key space).
    ///
    /// # Step 4 (2026-08-09): the clear path H-2 left open, and its trust root
    ///
    /// H-2 removed the last automatic reset and left "restart cImp" as the only
    /// escape — which is why the field doc used to end there. What it did not
    /// settle is what *may* clear the bit, and the answer is not a better piece
    /// of evidence: it is **authority**. A human acting in cImp's own UI is a
    /// fact no shell can fabricate (the webview holds no bearer token, and
    /// [`apply_latch_override`] is reachable only from the capability-scoped
    /// `latch_override` IPC command), and it is the same trust root every other
    /// consent surface in this app already uses.
    ///
    /// So exactly three things clear it, all rooted in that click:
    ///
    /// 1. [`LatchOverride::ClearContamination`] — the user judged the flagged
    ///    content harmless. Cleared immediately; nothing else about the tab or
    ///    its session changes.
    /// 2. [`LatchOverride::AwaitSessionClear`] +
    ///    [`awaiting_session_clear`](Self::awaiting_session_clear) — the user
    ///    restored a checkpoint. The bit **stays set** and lifts only when a
    ///    proved session rotation is observed. See that field for why a forgeable
    ///    rotation signal is acceptable *there* and nowhere else.
    /// 3. [`LatchOverride::Unlatch`] — the user restored FULL access. See "The
    ///    2026-08-10 amendment" below for the argument.
    ///
    /// **The accepted cost is unchanged for everything else.** A genuine
    /// `/clear` in a tab nobody armed keeps the bit: that conversation's
    /// `context_note` writes stay quarantined (they are stored and held for
    /// review, not dropped) and the badge keeps saying "contaminated".
    /// [`LatchOverride::FlipLocal`] — the workflow flip — still cannot clear it,
    /// and neither can any HTTP route: `/latch/beacon` only ever tightens, and
    /// `POST /latch/override` has not existed since #45.
    ///
    /// # The 2026-08-10 amendment: a full unlatch IS a verdict
    ///
    /// **The 2026-08-10 amendment to decision 15** (user: *"if the user restores
    /// full access then the tab should be cleared, it's the user's decision."*).
    /// A full unlatch hands back read AND web with the injected content still in
    /// the context window; that is the strictly larger risk, and it is taken
    /// behind the popover's own confirmation. Leaving the *memory* half
    /// quarantined after it made the product overrule a judgement it had just
    /// asked the user to make. Same trust root as (1): authority, not evidence.
    ///
    /// **Clearing the STATE never erases the EVIDENCE.** The
    /// [`outbound::Screen::Contamination`] row that set the bit stays in its own
    /// retention lane, and every release writes an
    /// [`outbound::Screen::ContaminationCleared`] row beside it — including the
    /// unlatch's ([`unlatch_clear_row`]). "Cleared" and "never contaminated" are
    /// therefore distinguishable in the feed even though the live view is
    /// identical, which is the point.
    pub(super) contaminated: bool,
    /// Step 4: the **one-shot arm** — the only thing that lets a session
    /// rotation clear [`contaminated`](Self::contaminated).
    ///
    /// Set by [`LatchOverride::AwaitSessionClear`], i.e. by a user who restored
    /// a checkpoint. Consumed by [`observe`](Self::observe) the next time a
    /// changed session id arrives, which clears the contamination bit and
    /// disarms in the same move. Also cleared by an immediate
    /// [`LatchOverride::ClearContamination`], which supersedes it (there is
    /// nothing left to wait for).
    ///
    /// # Why restore does not simply clear, and why the arm is safe
    ///
    /// **Restore is the case where clearing is *least* justified.** Rolling back
    /// files cannot remove injected text from the model's context window, so the
    /// conversation the user is worried about is still running. The UI therefore
    /// tells them to `/clear`, and cImp waits until it sees that happen.
    ///
    /// **And this is the one place a filesystem-derived rotation signal may be
    /// trusted.** H-2's argument is intact: a Claude tab's session id comes from
    /// a directory the model's own Bash can write, so the signal is forgeable.
    /// What changes here is what the signal *decides*. It is not carrying the
    /// decision — the click is. An attacker cannot click restore, so a forged
    /// rotation only helps in the case where the user has **already decided** the
    /// bit should go; the worst outcome is that it lifts slightly earlier than
    /// their actual `/clear`. The signal answers "has the authorised thing
    /// happened yet?", never "should it happen?". H-2's decode proof still gates
    /// it: `observe` only ever sees session ids the live-session registry
    /// published, and that registry takes Claude ids from
    /// `harness::claude::read::LiveSessionGate` (a decoded record naming the session) and
    /// OpenCode ids from a `session.created` on the harness's own event stream.
    ///
    /// # Lifetime
    ///
    /// It lives in the registry entry, so it has the entry's lifetime: it
    /// survives a tab restart (which is itself a rotation, and therefore fires
    /// it) and dies with the process. An app restart drops the whole entry —
    /// contamination included — so an arm outliving one is meaningless by
    /// construction.
    pub(super) awaiting_session_clear: bool,
    /// #48 (F-23): this tab's `local` latch was put there by the USER's
    /// [`LatchOverride::FlipLocal`] click — it was **not** earned by a
    /// local-capability tool call.
    ///
    /// # Why the position alone is not enough
    ///
    /// `Latch::Local` has two causes and the refusals for them are different
    /// statements. Reached by [`Latch::engage`] it means *"this session read a
    /// file, so the web side closed"*; reached by decision 15's workflow flip it
    /// means *"a human closed the web side and handed local capability back"*.
    /// The web-direction refusal used to serve the first sentence in both cases
    /// ([`toolclass::REFUSAL_NATIVE_WEB_BLOCKED`]), which is F-23: a refusal
    /// stating a cause it did not check. The gate cannot recover that cause from
    /// [`Latch`] — the enum is a position, not a history — so the fact is
    /// recorded here at the one site that performs the flip, beside the
    /// [`outbound::Screen::LatchOverride`] row that records the same act for the
    /// audit trail.
    ///
    /// # Lifetime, and why it cannot outlive its latch
    ///
    /// Set **only** in `apply_override`'s [`LatchOverride::FlipLocal`] arm, and
    /// cleared everywhere the latch leaves `Local`: the [`LatchOverride::Unlatch`]
    /// arm and [`observe`](Self::observe)'s rotation reset. Those are the only
    /// three writes to [`latch`](Self::latch) in this module, so the field cannot
    /// describe a latch position that is no longer in force — a stale `true` on a
    /// re-latched `local` would attribute a tool call's latch to the user, which
    /// is F-23 with the operands swapped.
    ///
    /// It is deliberately **not** a "the user touched this tab" flag: `Unlatch`,
    /// `ClearContamination` and `AwaitSessionClear` are user actions too and none
    /// of them leaves the latch `local`. What this answers is exactly one
    /// question — *why is the web side closed?* — and it is read by exactly one
    /// consumer, the native-web direction of the Phase H gate, through
    /// [`LatchView::local_by_user_flip`].
    local_by_user_flip: bool,
}

impl TabLatch {
    /// A brand-new, uncontaminated entry. One constructor so a field added
    /// later cannot be initialized two different ways at the two sites that
    /// create rows (`gate` and the Phase F `beacon`).
    pub(super) fn fresh() -> Self {
        TabLatch {
            session: None,
            latch: Latch::Open,
            budget: Budget::default(),
            latch_flagged: false,
            beacon_flagged: false,
            contaminated: false,
            awaiting_session_clear: false,
            local_by_user_flip: false,
        }
    }

    /// The user-facing projection of this entry (Phase F): what the badge
    /// shows and which override buttons the popover may enable.
    fn view(&self) -> LatchView {
        LatchView {
            latch: self.latch.label(),
            contaminated: self.contaminated,
            // Decision 15's two moves, as availability rather than as UI
            // knowledge: the frontend must not re-derive "when is flip legal"
            // from the label, or the rule would live in two places and drift.
            can_flip_local: self.latch == Latch::External,
            can_unlatch: self.latch != Latch::Open,
            // Step 4's two, published on the same principle. `can_clear` is
            // deliberately not `contaminated` spelled twice in TypeScript: the
            // legality rule for a move belongs to the backend even when it is
            // currently one field wide.
            can_clear: self.contaminated,
            awaiting_session_clear: self.awaiting_session_clear,
            // F-23: published rather than re-derived, for the same reason
            // `can_flip_local` is — a consumer that inferred "the user must have
            // flipped it" from `latch == "local" && contaminated` would be
            // guessing, and would be wrong for the tab that fetched a page,
            // latched EXTERNAL and was never flipped at all.
            local_by_user_flip: self.local_by_user_flip,
        }
    }

    /// Step 4: **the one place [`contaminated`](Self::contaminated) is
    /// cleared.** Returns what the tab looked like immediately before, or `None`
    /// when it was not contaminated at all.
    ///
    /// All three authorised paths funnel through here — the user's immediate
    /// resume, the full unlatch (2026-08-10 amendment) and the armed rotation —
    /// so a field that has to be reset alongside the bit cannot be reset on one
    /// path and forgotten on the other.
    ///
    /// # What it resets, and why the two report bits are not optional
    ///
    /// `latch_flagged` and `beacon_flagged` are one-row-per-scope claim bits:
    /// once set, the refusal and beacon rows they gate are never written again
    /// for this tab-session. Leaving them set across a clear would make a
    /// **re-contamination silent** — the tab would take external content again
    /// and the feed would show nothing new, which is exactly the class of bug
    /// #48 fixed for the `Local`-latched beacon. Clearing the bit means this tab
    /// gets to report its containment events afresh.
    ///
    /// (The `contamination` row itself is self-limiting through
    /// [`note_contamination`]'s `mem::replace`, so clearing the bit re-arms that
    /// one automatically — which is what the re-contamination test asserts,
    /// rather than asserting these two booleans directly.)
    ///
    /// # What it deliberately does NOT touch
    ///
    /// * **The latch.** Resume "changes nothing else"; the latch has its own two
    ///   buttons and its own rules. Leaving it where it is can only be the
    ///   tighter choice. (The unlatch path moves the latch too — but it does so
    ///   in its own arm of [`LatchRegistry::apply_override`], *after* this
    ///   function has run, so `PriorTaint::latch` still names the latch the bit
    ///   was released from. This function never touches the latch on any path.)
    /// * **The budget.** Spend is not a report flag — the same reason
    ///   [`LatchRegistry::apply_override`] does not refill it.
    /// * **Quarantined notes.** Locked decision 10 keeps promote-or-discard
    ///   behind the Memory view's own review, which is a separate consent
    ///   surface. Clearing the tab bit stops *future* writes being quarantined;
    ///   notes already held stay held. Nothing in this module can reach them, and
    ///   that is the point.
    fn clear_contamination(&mut self) -> Option<PriorTaint> {
        if !self.contaminated {
            // An arm can only be set on a contaminated tab, but if one ever
            // outlived its bit it would be a trap waiting to fire on the next
            // rotation. Drop it.
            self.awaiting_session_clear = false;
            return None;
        }
        let prior = PriorTaint {
            latch: self.latch.label(),
            armed: self.awaiting_session_clear,
            session: self.session.clone(),
        };
        self.contaminated = false;
        self.awaiting_session_clear = false;
        self.latch_flagged = false;
        self.beacon_flagged = false;
        Some(prior)
    }

    /// Fold the currently-observed session id into this entry, resetting the
    /// latch when the tab's session has demonstrably **rotated**.
    ///
    /// This is what makes the LATCH's scope "the tab's live session" rather
    /// than "the tab": a tab restart starts a new harness session, the V28
    /// registry re-stamps the tab with the new id, and the new conversation is
    /// not denied `read`/`bash` on the strength of the previous one's fetch.
    /// (The tab id itself never rotates — it is config-derived — so keying on
    /// it alone would strand a tab latched until the app restarted.)
    ///
    /// **What a rotation does NOT do (H-2).** It does not clear
    /// [`contaminated`](Self::contaminated). Everything reset here is
    /// permissive state that the next real call re-earns; the contamination bit
    /// is the one fact whose reset an attacker would *want*, and the rotation
    /// signal is derived from a file that attacker can create. See the field's
    /// doc for the full argument and its accepted cost.
    ///
    /// **The one exception, and it is not an exception to that argument (step
    /// 4).** If — and only if — the user has armed
    /// [`awaiting_session_clear`](Self::awaiting_session_clear) by restoring a
    /// checkpoint, this rotation is the awaited event and the bit is cleared
    /// here. H-2 is untouched: an unarmed tab is unchanged by any rotation,
    /// forged or genuine, which is what
    /// `a_session_rotation_resets_the_latch_but_never_the_contamination_bit`
    /// still asserts. The arm is the authority; the rotation only says "yet".
    ///
    /// The three cases, and why `None` is not one of them:
    /// - a *different* session id ⇒ new scope, latch back to [`Latch::Open`];
    /// - the *same* id ⇒ unchanged;
    /// - **no** id ⇒ unchanged, and the stored id is kept. The registry
    ///   withholds a session for reasons that have nothing to do with restarts
    ///   (TTL staleness, the H1 same-root ambiguity, a tab that has not yet
    ///   emitted a session-bearing event). Treating that silence as a restart
    ///   would hand an injected model a trivial latch reset: keep calling until
    ///   the registry blinks.
    #[must_use = "an armed tab clears its contamination bit here and the clear owes the feed a \
                  row — call ContaminationCleared::record() after dropping the registry lock"]
    pub(super) fn observe(&mut self, session: Option<&str>) -> Option<ClearedOnRotation> {
        let s = session?;
        match self.session.as_deref() {
            Some(prev) if prev == s => None,
            Some(prev) => {
                let prior_session = prev.to_string();
                // Captured before the resets below, because it is what the audit
                // row means by "prior state".
                let prior_latch = self.latch.label();
                self.session = Some(s.to_string());
                self.latch = Latch::Open;
                // #48 (F-23): the latch this described is gone, so the reason it
                // was in that position goes with it. Left set, a `local` latch
                // re-earned by the next conversation's file read would be
                // reported as the user's decision.
                self.local_by_user_flip = false;
                // V32 Phase C: the new conversation gets a fresh budget and a
                // fresh right to report — same scope, same reset.
                self.budget.reset();
                self.latch_flagged = false;
                self.beacon_flagged = false;
                // H-2 (2026-08-08 re-review): `contaminated` is deliberately
                // NOT cleared here — see the field's own doc. A rotation is a
                // claim about a file an attacker can create; it may reopen the
                // latch and refill the budget (both merely permissive, and both
                // re-earned by the next real call), but it may not un-taint a
                // context window.
                //
                // Step 4: unless the USER armed this exact wait. The guard is
                // the whole design — it is checked before anything is cleared,
                // so a rotation into an unarmed tab takes the H-2 path above and
                // nothing else. Deliberately not `if let Some(..) = ..` over
                // `clear_contamination`: that call must not run at all on an
                // unarmed tab, or a later refactor of it could start reaching
                // the bit through this door.
                if !self.awaiting_session_clear {
                    return None;
                }
                let prior = self.clear_contamination()?;
                Some(ClearedOnRotation {
                    prior_latch,
                    prior_session,
                    session: s.to_string(),
                    armed: prior.armed,
                })
            }
            // First sighting: the same scope, only now identified. The latch
            // carries over — calls made before the registry knew the session
            // still happened in this conversation.
            //
            // Not a rotation, so it cannot fire the arm either: "we did not know
            // the id before" is not evidence that the conversation changed. This
            // is the same reading `None` gets above.
            None => {
                self.session = Some(s.to_string());
                None
            }
        }
    }
}

/// What a tab looked like immediately before [`TabLatch::clear_contamination`]
/// released it — the "prior state" every clear's audit row records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PriorTaint {
    /// [`Latch::label`] at the moment of the clear — i.e. the latch the bit was
    /// released *from*. `clear_contamination` never changes it; on the unlatch
    /// path the caller moves the latch to `Open` immediately afterwards, so this
    /// reads `external`/`local`, which is the state an audit row means by
    /// "prior". It therefore always equals `OverrideOutcome::prior.label()`.
    pub(super) latch: &'static str,
    /// Whether the one-shot arm was set. False for a false-positive resume of an
    /// un-armed tab; true when the clear is a restore's arm firing.
    pub(super) armed: bool,
    /// The conversation the tab was in. For the rotation path this is the
    /// *outgoing* session — the contaminated one.
    pub(super) session: Option<String>,
}

/// A contamination bit cleared inside [`TabLatch::observe`] — the armed
/// one-shot firing on a proved session rotation.
///
/// Returned rather than recorded in place for the reason [`Contamination`] is:
/// the transition happens under the registry mutex and `record_flag` does file
/// I/O. Every caller of `observe` turns this into a
/// [`ContaminationCleared`] with its own [`LatchScope`] — which is also why the
/// scope is not carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a contamination clear that is not recorded is an unaudited release of containment"]
pub(super) struct ClearedOnRotation {
    /// [`Latch::label`] before the rotation reopened it.
    prior_latch: &'static str,
    /// The contaminated conversation that just ended.
    prior_session: String,
    /// The conversation the tab is now in.
    session: String,
    /// Always true — the arm is the only way to reach this type. Carried so the
    /// row builder takes the same `armed` input on both paths.
    armed: bool,
}

/// V32 Phase F: the containment state of one tab, as the badge and the
/// override popover need it. Shared by `/status`, the `latch_status` IPC
/// command and the two Phase F endpoints' replies so all four describe a tab
/// with the same four facts.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatchView {
    /// [`Latch::label`]: `open` / `external` / `local`.
    pub latch: &'static str,
    /// Locked decision 15: has external content entered this conversation at
    /// all? Survives [`LatchOverride::FlipLocal`] and — since H-2 — every
    /// *unarmed* session rotation: the bit is sticky for the tab's registry
    /// entry. It is released only by the three USER actions listed on
    /// [`TabLatch::contaminated`], one of which is
    /// [`LatchOverride::Unlatch`] (2026-08-10 amendment).
    pub contaminated: bool,
    /// Whether "switch to local" applies right now (EXTERNAL-latched only).
    pub can_flip_local: bool,
    /// Whether "restore full access" applies right now (anything but open).
    pub can_unlatch: bool,
    /// Step 4: whether either contamination clear applies right now — i.e.
    /// whether the tab is contaminated at all.
    pub can_clear: bool,
    /// Step 4: whether the user has armed the one-shot clear (they restored a
    /// checkpoint) and cImp is waiting for this tab to start a new harness
    /// session. See [`TabLatch::awaiting_session_clear`].
    ///
    /// Published because the UI has to say *why* a contaminated tab is showing
    /// no "clear now" affordance after a restore, and because step 5's
    /// restore-linked entry point must be able to tell an already-armed tab from
    /// a fresh one without re-deriving the rule.
    pub awaiting_session_clear: bool,
    /// #48 (F-23): whether [`latch`](Self::latch) reads `local` because the USER
    /// flipped it there, rather than because a local-capability tool ran. See
    /// [`TabLatch::local_by_user_flip`].
    ///
    /// Published on `/latch/state` so the OpenCode plugin's web-direction refusal
    /// can serve the constant whose cause it actually checked. It is a fact cImp
    /// recorded when it applied the override, so selecting a message with it is a
    /// lookup — not a message composed from anything a caller supplied.
    ///
    /// `false` for every latch that is not `local`, by construction: the three
    /// writes to the underlying latch keep the two in step.
    pub local_by_user_flip: bool,
}

impl Default for LatchView {
    /// The view of a tab the proxy has never served: nothing latched, nothing
    /// contaminated, no override available.
    fn default() -> Self {
        LatchView {
            latch: Latch::Open.label(),
            contaminated: false,
            can_flip_local: false,
            can_unlatch: false,
            can_clear: false,
            awaiting_session_clear: false,
            local_by_user_flip: false,
        }
    }
}

/// The USER-initiated containment moves — V32 Phase F's two latch moves
/// (locked decision 15), plus step 4's two contamination moves.
///
/// There is still no "latch external": the system does that, and an action that
/// only ever tightens needs no consent surface. What step 4 adds is the *clear*
/// this enum's doc used to say could not exist. H-2's conclusion was that no
/// filesystem-derived **evidence** may un-taint a context window, and that
/// stands; the trust root here is not evidence but **authority** — a human
/// acting in cImp's own UI (see [`TabLatch::contaminated`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatchOverride {
    /// EXTERNAL → Local. Restores the proxied local-capability tools and closes
    /// the external side in the same move.
    FlipLocal,
    /// Anything → Open. Restores both sides; the UI puts a confirmation in
    /// front of it because it recreates the trifecta with injected content
    /// still in the context window.
    ///
    /// **And it clears [`TabLatch::contaminated`]** — decision 15's 2026-08-10
    /// amendment. The click is a verdict, not a workflow step: it already hands
    /// back the strictly more dangerous capability, so quarantining persistent
    /// memory afterwards overruled the user's own decision. On an
    /// uncontaminated tab it clears nothing and is still legal; the clear is a
    /// consequence of the action, not its precondition (contrast
    /// [`ClearContamination`](Self::ClearContamination), whose entire purpose is
    /// the clear, so "nothing to clear" is *its* error).
    Unlatch,
    /// Step 4 — **false-positive resume.** The user has looked at what was
    /// flagged and judged it harmless. Clears the contamination bit now; the
    /// session, the tab and the working tree are untouched (no restart, no
    /// `/clear`, no file written). The UI puts a confirmation in front of it for
    /// the same reason it does for [`Unlatch`](Self::Unlatch): if the judgement
    /// is wrong, a steered model gets its persistence channel back.
    ClearContamination,
    /// Step 4 — **restore.** The user rolled files back to a checkpoint. That
    /// cannot remove injected text from the model's context window, so this
    /// clears **nothing**: it arms
    /// [`TabLatch::awaiting_session_clear`], and the bit lifts only once cImp
    /// observes the tab start a new harness session.
    AwaitSessionClear,
}

impl LatchOverride {
    /// Parse a wire value. An unrecognized action is an **error**, never a
    /// benign default: the actions differ in exactly how much capability they
    /// hand back, so a typo must not pick one.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "flip_local" => Ok(LatchOverride::FlipLocal),
            "unlatch" => Ok(LatchOverride::Unlatch),
            "clear_contamination" => Ok(LatchOverride::ClearContamination),
            "await_session_clear" => Ok(LatchOverride::AwaitSessionClear),
            other => Err(format!(
                "invalid latch override `{other}` — expected one of \"flip_local\", \"unlatch\", \
                 \"clear_contamination\", \"await_session_clear\""
            )),
        }
    }

    /// The canonical wire value, and the `tool` column of the activity row.
    pub fn as_str(self) -> &'static str {
        match self {
            LatchOverride::FlipLocal => "flip_local",
            LatchOverride::Unlatch => "unlatch",
            LatchOverride::ClearContamination => "clear_contamination",
            LatchOverride::AwaitSessionClear => "await_session_clear",
        }
    }
}

/// The result of an applied override: what the latch was, and what the tab
/// looks like now. The prior state is carried because it is the fact the
/// activity row exists to record — "restored full access" means something very
/// different from `external` than from `local`.
#[derive(Debug)]
pub(super) struct OverrideOutcome {
    pub(super) prior: Latch,
    /// Step 4: the taint state before the move, for the same reason `prior`
    /// exists — "cleared the contamination flag" is only legible beside what was
    /// there. `Some` **only when this override actually released the bit**:
    /// `clear_contamination`, or an `unlatch` on a contaminated tab (decision
    /// 15's 2026-08-10 amendment). `None` for `flip_local`, for the arm, and for
    /// any move on an uncontaminated tab — which is what
    /// [`unlatch_clear_row`] keys its "write no row" decision on.
    pub(super) prior_taint: Option<PriorTaint>,
    pub(super) view: LatchView,
}

/// The result of a native-web beacon (#45): the tab's resulting view, which of
/// the two state changes it caused, and whether it is this tab-session's
/// reportable one (#48).
///
/// `report` is what makes the beacon's audit row bounded, and it is a stored
/// bit ([`TabLatch::beacon_flagged`]) rather than a derived one. A caller that
/// POSTs the route in a loop produces one row per tab-session, not one per
/// request; a feed a caller can flood is a feed that evicts the rows it exists
/// to keep.
///
/// #45 derived that bound from `engaged` alone, which silently dropped a whole
/// class of beacon — see [`contaminated_now`](Self::contaminated_now).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct BeaconOutcome {
    pub(super) view: LatchView,
    /// The latch itself MOVED: Open → External. False when it was already
    /// External (sticky, and the fact is unchanged) **and** when the tab was
    /// latched `Local`, where the beacon cannot move it at all.
    pub(super) engaged: bool,
    /// This beacon is what made the conversation contaminated — the bit went
    /// `false` → `true` here.
    ///
    /// #45 wrote a row only `if engaged`, so a beacon aimed at a `Local`-latched
    /// tab set `contaminated` unconditionally and recorded **nothing**: no row,
    /// no `warn!`, no `info!`. From that moment every `context_note` in the tab
    /// is quarantined and every external result enveloped, with the only
    /// evidence being the quarantine rows of the *later* writes. Locked decision
    /// 15 is unmoved — this records that the bit was SET, and nothing here or
    /// anywhere else clears it.
    pub(super) contaminated_now: bool,
    /// Whether the handler should write this beacon's `injection_flag` row:
    /// something changed (`engaged || contaminated_now`) **and** this
    /// tab-session has not reported a beacon yet.
    pub(super) report: bool,
}

impl BeaconOutcome {
    /// Nothing was touched: no policy in force, or no scope to engage.
    pub(super) fn inert() -> Self {
        BeaconOutcome {
            view: LatchView::default(),
            engaged: false,
            contaminated_now: false,
            report: false,
        }
    }
}

/// Which tool-serving route a gate call is running for. The two differ in one
/// respect only: what an [`ToolClass::External`] classification *means* there.
///
/// `pub(super)` since #48 because the **worker** needs the same distinction and
/// was making the decision without it (review finding A-1). One definition of
/// the rule, in the module that first got it right, rather than a second copy
/// in `agent.rs` that can drift from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LatchRoute {
    /// A proxied `<server>__<tool>` id: `/mcp/call`, and the worker's
    /// MCP-host branch. Every name here is namespaced and therefore EXTERNAL
    /// by the Phase A unknown-⇒-EXTERNAL invariant; this route is the
    /// untrusted-content intake.
    Proxied,
    /// A cImp-native bare name: `/graph_run`'s `graph_*` / `context_*` tools,
    /// and the worker's native dispatch. This route physically cannot serve a
    /// proxied server's content, so a name that classifies EXTERNAL here is not
    /// external content at all: it is a typo or a hallucination that dispatch
    /// will reject as unknown. Letting it *engage* the latch would let one bad
    /// tool name poison a scope for its whole session — so on this route
    /// EXTERNAL neither latches nor is refused, and dispatch answers with its
    /// own error.
    Native,
    /// A **cImp-initiated hook**: the three `/context/*` shim routes (#48,
    /// finding M-7). Like [`LatchRoute::Native`] in what an EXTERNAL
    /// classification means — these routes serve fixed, cImp-owned names and
    /// physically cannot carry a proxied server's content — and different in
    /// exactly one respect, which is the whole reason the variant exists:
    /// **a hook may be REFUSED by a latch but must never MOVE one.**
    ///
    /// The calls arriving here are not tool calls. `PreToolUse`/`PreCompact`/
    /// `PostToolUse` fire automatically, for cImp's own automation, over work
    /// the harness has *already* permitted. Letting them engage would latch
    /// every tab with the read advisor or auto-check enabled to `Local` at its
    /// first read or edit, and every proxied web/MCP tool would be refused from
    /// that moment for a choice the model never made. The latch records what a
    /// CONVERSATION elected to do; cImp advising on a read the model had
    /// already been granted is not the conversation electing anything.
    ///
    /// It still *reads* the latch, because the other direction is the one M-7
    /// is about: under an EXTERNAL latch cImp must not execute the project's
    /// configured checks, or hand back repo source text, on behalf of a
    /// conversation that has ingested untrusted content.
    Hook,
    /// **V39 Phase B: a cross-harness delegation** (`POST /delegate`).
    ///
    /// The third point in the route space, and it needs its own variant
    /// because it is the one combination the other three cannot express:
    ///
    /// | | name comes from | may be REFUSED | may MOVE the latch |
    /// |---|---|---|---|
    /// | [`Proxied`](Self::Proxied) | the model | yes | yes |
    /// | [`Native`](Self::Native) | the model | yes | yes |
    /// | [`Hook`](Self::Hook) | **cImp** | yes | **no** |
    /// | `Delegation` | **cImp** | yes | **yes** |
    ///
    /// *Name from cImp*, like a hook: the model names `delegate_task_<harness>`
    /// on the child, which resolves the harness id and forwards THAT — the
    /// route states its own class-table identity ([`DELEGATE_TOOL`]) and takes
    /// no tool name from the request. So M-2's "in the table but not
    /// dispatchable" wave-through must not apply here, exactly as it must not
    /// on `Hook`: the name is the route, not a hallucination dispatch will
    /// reject.
    ///
    /// *Elective, unlike a hook*: a hook fires automatically over work the
    /// harness already permitted, which is why it must not latch. A
    /// `delegate_task_*` call is the conversation choosing to hand work to a
    /// peer and take its answer back — as elective as `offload_task`, and
    /// latching for the same reason.
    Delegation,
}

impl LatchRoute {
    /// The route a tool name arrives on, by the one convention both dispatchers
    /// use: a namespaced `<server>__<tool>` id is proxied, a bare name is
    /// native (`agent.rs::HostRouter::call`, `mcp_host::call_for_consumer`).
    ///
    /// Never answers [`LatchRoute::Hook`] or [`LatchRoute::Delegation`]: both
    /// are properties of the ROUTE and not of the name, so those handlers state
    /// it themselves.
    pub(super) fn of_tool(name: &str) -> Self {
        if name.contains("__") {
            LatchRoute::Proxied
        } else {
            LatchRoute::Native
        }
    }

    /// Whether an admitted call on this route may **move** the scope's latch.
    ///
    /// `false` only on [`LatchRoute::Hook`] — see that variant. A separate axis
    /// from [`external_is_content`](Self::external_is_content), deliberately: a
    /// hook has to be classified and gated (so it can be refused) without being
    /// elective (so it must not latch). [`LatchRoute::Delegation`] shares the
    /// hook's *name-from-cImp* property and not this one, which is exactly why
    /// it is a fourth variant rather than a reuse of either neighbour.
    pub(super) fn engages(self) -> bool {
        self != LatchRoute::Hook
    }

    /// Whether an [`ToolClass::External`] classification on this route really
    /// means **external content**.
    ///
    /// `false` on [`LatchRoute::Native`], [`LatchRoute::Hook`] and
    /// [`LatchRoute::Delegation`] is the whole rule, and it is not a
    /// weakening of the unknown-⇒-EXTERNAL invariant: every proxied id contains
    /// `__` by construction, so the restrictive default still governs every
    /// name that can carry external content. What it excludes is the name that
    /// cannot — a misspelled `graph_symbols`, which is a hallucination, not a
    /// page.
    pub(super) fn external_is_content(self) -> bool {
        self == LatchRoute::Proxied
    }

    /// **Whether a gated call could actually EXECUTE on this route** — i.e.
    /// whether there is anything for the latch to be about. `false` means the
    /// gate must return without refusing and without moving the latch, and let
    /// the dispatcher answer with its own unknown-tool error.
    ///
    /// Two rules, one predicate, because they are the same principle applied to
    /// the two ways a name can fail to name a tool on a native route (#48,
    /// findings A-1 and M-2):
    ///
    /// 1. **Not in the table** — `class == External` on a route that cannot
    ///    carry a proxied server's content
    ///    ([`external_is_content`](Self::external_is_content)). A misspelled
    ///    `graph_symbols` is a hallucination, not a page.
    /// 2. **In the table but not dispatchable**
    ///    ([`toolclass::dispatchable`]). Six names are classified for reasons
    ///    other than being callable — the three `/context/*` hook routes' fixed
    ///    identities and Claude's own `Edit`/`Write`/`Bash`. Before this, a
    ///    model emitting the bare name `hook_post_edit` or `Bash` on
    ///    [`LatchRoute::Native`] classified LOCAL-CAPABILITY and latched the
    ///    scope to `Local` **before** dispatch rejected the name: the A-1 harm
    ///    (one bad tool name costs a scope the other half of its tools) in the
    ///    direction A-1's fix did not cover.
    ///
    /// Rule 2 is deliberately confined to [`LatchRoute::Native`], the only
    /// route whose name is model-supplied. [`LatchRoute::Hook`]'s name is
    /// composed by cImp and *is* the route's identity — applying rule 2 there
    /// would wave through the three hook routes M-7 exists to gate — and
    /// [`LatchRoute::Proxied`]'s names are the MCP host's to reject.
    ///
    /// This is not a weakening of unknown-⇒-EXTERNAL: it never admits a name
    /// into a *less* restrictive class, it only declines to record taint for a
    /// call that never runs. The containment question — may this class run at
    /// all — is still [`Latch::refusal`]'s, and every name that answers `true`
    /// here still faces it.
    pub(super) fn can_execute(self, name: &str, class: ToolClass) -> bool {
        if class == ToolClass::External && !self.external_is_content() {
            return false;
        }
        self != LatchRoute::Native || toolclass::dispatchable(name)
    }
}

/// What the calling ROUTE knows about a gated call that the registry cannot see
/// (#48, finding F-3): who asked for it, and — when the call is an intake —
/// where the content it is about to bring back is coming from.
///
/// Every field here is one the [`Screen::Contamination`](outbound::Screen)
/// row needs and the [`LatchRegistry`] has no way to derive. The registry owns
/// per-tab state; it does not see request bodies, does not parse tool
/// arguments, and cannot tell an IPC command from a loopback POST.
///
/// **Required at every call site, not defaulted.** The same rule
/// [`outbound::Flag::origin`] is under, for the same reason: #45 found that a
/// provenance column behind a defaulting constructor lets a new call site
/// inherit "cImp decided this" by writing nothing, which is the exact shape of
/// omission these rows exist to prevent. A native route states
/// [`CallProvenance::internal`] — with no URL, because a native route cannot
/// carry a fetched page — as a decision rather than by omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallProvenance<'a> {
    /// Who asked for the state change a resulting row records.
    origin: outbound::Origin,
    /// The URL the call is fetching, when the route can see one. `/mcp/call`
    /// reads it out of the tool arguments (`detection::origin_of`) — the same
    /// pair its SSRF and detection rows carry, so a contamination row and the
    /// screen rows about the same call name the same page.
    url: Option<&'a str>,
    /// That URL's host — the at-a-glance column.
    host: Option<&'a str>,
    /// #48 F-16: the project this call runs against, in
    /// [`crate::activity::root_key`] form, for a row the registry writes when it
    /// has no scope to take one from ([`unattributed_write`]). `None` where the
    /// route genuinely has no project in view — `/latch/beacon` and the IPC
    /// override are about a tab, not a directory.
    ///
    /// Why the ROUTE and not the registry: [`LatchScope::root`] comes from the
    /// TAB (`tab_root_key`, F-3) precisely so a request body cannot redirect a
    /// contamination row. This field is the case where there IS no tab, and the
    /// only project in view is the one the call is about to write into — the same
    /// one the call's own `kind:"graph"` row files under, resolved by the same
    /// function (`GraphService::graph_root_key`), so the two rows for one call
    /// cannot name different projects.
    root: Option<&'a str>,
}

impl<'a> CallProvenance<'a> {
    /// cImp's own dispatch, executing a call it was already running, with no
    /// fetched content in view and no project in view either. Native routes that
    /// are about a TAB rather than a directory.
    pub(super) const fn internal() -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url: None,
            host: None,
            root: None,
        }
    }

    /// cImp's own dispatch on a native route that knows which project the call
    /// runs against (`/graph_run`). See [`Self::root`].
    pub(super) const fn internal_in(root: &'a str) -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url: None,
            host: None,
            root: Some(root),
        }
    }

    /// cImp's own dispatch over the proxied intake, naming the page it is
    /// about to read (either half may be absent — a search tool has arguments
    /// but no URL).
    ///
    /// No `root`: a PERSISTENT-WRITE cannot arrive on `/mcp/call` (every
    /// namespaced id classifies EXTERNAL), so the one row that reads
    /// [`Self::root`] is unreachable from here and a root passed in would be
    /// speculative.
    pub(super) fn intake(url: Option<&'a str>, host: Option<&'a str>) -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url,
            host,
            root: None,
        }
    }

    /// A loopback POST from a local process — the native-web beacon. Marked
    /// [`outbound::Origin::Http`] because the launch token is readable by
    /// anything running as this user, so a beacon is never evidence that the
    /// user acted (#45).
    pub(super) const fn http() -> Self {
        CallProvenance {
            origin: outbound::Origin::Http,
            url: None,
            host: None,
            root: None,
        }
    }
}

/// The per-tab-session taint latches for the tools this proxy serves.
///
/// Locked decision 3: consumer enforcement lives here, keyed by V28 tab
/// identity. Two asymmetries with the worker's latch are deliberate:
///
/// - **Refusal, not def removal.** The worker rebuilds its advertised tool list
///   every turn, so decision 2's def removal is available to it. Consumers
///   cache `tools/list` at connect (the long-standing OpenCode behaviour that
///   forces a tab restart after MCP flag changes, and Claude does the same), so
///   removing a def mid-session would not be seen. The fixed-string refusals
///   from [`toolclass`] are the whole enforcement here.
/// - **Only the tools this proxy serves.** Claude's native Read/Bash and
///   OpenCode's bash/write never route through cImp, so no latch of ours can
///   reach them (decision 3's honest limit; OS containment is V33, optional
///   hook gating is Phase E).
#[derive(Default)]
pub(super) struct LatchRegistry {
    tabs: Mutex<HashMap<(&'static str, String), TabLatch>>,
}

/// V32 Phase G (locked decision 16): the two feature switches one gated call
/// resolves, snapshotted by the handler that owns the settings read.
///
/// They are separate because they *are* separate features and can be switched
/// independently — and the interesting combination is the asymmetric one:
/// latch off + quarantine on still tracks contamination (so a note written
/// after a fetch is still held for review) while refusing nothing. The registry
/// takes them as data rather than reading settings itself, so the whole gate
/// stays a pure decision over one lock and one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GatePolicy {
    /// [`Feature::TaintLatch`](crate::settings::injection::Feature::TaintLatch)
    /// — engagement, refusals and the latch shown in `/status`.
    pub(super) latch: bool,
    /// [`Feature::MemoryQuarantine`](crate::settings::injection::Feature::MemoryQuarantine)
    /// — whether a PERSISTENT-WRITE from a contaminated conversation is stored
    /// held-for-review.
    pub(super) quarantine: bool,
}

impl GatePolicy {
    /// Resolve both switches for one tab scope. `None` scope ⇒ the unknown
    /// caller's answer, the same fail-open reading `Scope::for_tab` takes
    /// (#48 F-35: `Scope::UnknownCaller` is what `Scope::App` was called at this
    /// site before locked decision 36; no behaviour moved).
    pub(super) fn resolve(settings: &crate::settings::Settings, scope: Option<&LatchScope>) -> Self {
        use crate::settings::injection::{effective, Feature, Scope};
        let s = scope.map_or(Scope::UnknownCaller, LatchScope::injection);
        GatePolicy {
            latch: effective(Feature::TaintLatch, s, settings),
            quarantine: effective(Feature::MemoryQuarantine, s, settings),
        }
    }

    /// Neither control applies — nothing to decide, nothing to record.
    fn inert(self) -> bool {
        !self.latch && !self.quarantine
    }
}

/// One contamination TRANSITION, ready to record — see [`note_contamination`].
///
/// Owned rather than borrowed because the transition is detected under the
/// registry mutex and the row is written after it is dropped: `record_flag`
/// goes through `activity::record_bg`, whose contract is that it does file I/O
/// (inline off a tokio runtime), and holding a lock across that would put the
/// store's I/O on the critical path of every other tab's gated call.
pub(super) struct Contamination {
    origin: outbound::Origin,
    consumer: &'static str,
    /// `agent:tab` — [`LatchScope::label`], the same convention every other
    /// V32 row uses.
    scope: String,
    /// The conversation, when the registry entry knows it.
    session: Option<String>,
    tool: String,
    url: Option<String>,
    host: Option<String>,
    root: String,
    detail: String,
}

impl Contamination {
    /// Write the row. Fire-and-forget, like every other `record_flag` call on
    /// these paths: recording an event must not be able to fail the call it
    /// observes.
    fn record(self) {
        info!(
            target: "offload",
            consumer = self.consumer,
            scope = %self.scope,
            tool = %self.tool,
            host = self.host.as_deref().unwrap_or(""),
            root = %self.root,
            "loopback: V32 conversation became contaminated"
        );
        outbound::record_flag(outbound::Flag {
            screen: outbound::Screen::Contamination,
            origin: self.origin,
            consumer: self.consumer,
            scope: &self.scope,
            // #48 F-29: derived, because this row's `scope` was built by
            // `LatchScope::label` (or, with no tab identity, by the `/mcp/call`
            // route's honest `"{agent}:(no tab identity)"`) — the two inputs
            // `scope_attribution` is defined over. The struct carries the label
            // rather than the scope, so there is no tab field to read here.
            attribution: outbound::scope_attribution(&self.scope),
            session: self.session.as_deref(),
            tool: &self.tool,
            host: self.host.as_deref(),
            url: self.url.as_deref(),
            resolved_ip: None,
            canary: false,
            root: self.root.clone(),
            detail: &self.detail,
        });
    }
}

/// Step 4: on whose reasoning a contamination bit was released. The row's
/// `basis`, and the word that makes the audit trail legible — "the user said it
/// was a false positive" and "the user restored and then started a new session"
/// are very different claims about what is in the model's context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClearBasis {
    /// [`LatchOverride::ClearContamination`] — the user judged the flagged
    /// content harmless. Nothing else changed.
    Resume,
    /// [`LatchOverride::AwaitSessionClear`] armed the tab after a restore, and
    /// cImp has now observed a new harness session.
    Restore,
    /// [`LatchOverride::Unlatch`] — decision 15's 2026-08-10 amendment. The user
    /// restored FULL access, and the flag went with it. A third basis rather
    /// than a reuse of [`Resume`](Self::Resume) because the two are different
    /// claims about what the user decided: `Resume` says *"that content was
    /// harmless"*, this says *"I am taking the whole risk knowingly"* — and an
    /// incident reviewer who cannot tell them apart cannot reconstruct the
    /// decision.
    Unlatch,
}

impl ClearBasis {
    /// The row's at-a-glance `tool` column. These rows have no tool call behind
    /// them; what happened is the fact worth reading.
    pub(super) fn tool(self) -> &'static str {
        match self {
            ClearBasis::Resume => LatchOverride::ClearContamination.as_str(),
            ClearBasis::Restore => "session_clear_observed",
            // `"unlatch"` is also the `tool` column of the tab's
            // `latch_override` row. The two are told apart by `screen`, and
            // `contamination_events()` reads only the two contamination lanes,
            // so nothing joins them by accident — the shared word is a feature.
            ClearBasis::Unlatch => LatchOverride::Unlatch.as_str(),
        }
    }
}

/// One clearing of [`TabLatch::contaminated`], ready to record — the exact
/// counterpart of [`Contamination`], down to being owned rather than borrowed so
/// the row is written after the registry lock is dropped.
///
/// **Both authorised paths build one**, which is what stops the two from
/// describing the same state change differently: the immediate resume in
/// [`LatchRegistry::apply_override`], and the armed rotation firing inside
/// [`TabLatch::observe`] (via [`ClearedOnRotation::into_row`]).
pub(super) struct ContaminationCleared {
    /// Who acted *now*. [`outbound::Origin::Ipc`] for the resume — a human in
    /// the app's own UI. [`outbound::Origin::Internal`] for the armed rotation:
    /// the authority was the earlier click (which has its own
    /// [`outbound::Screen::LatchOverride`] row), but the act recorded here is
    /// cImp's own observation, and `Ipc` means "a human did *this*".
    pub(super) origin: outbound::Origin,
    pub(super) basis: ClearBasis,
    consumer: &'static str,
    /// `agent:tab` — [`LatchScope::label`].
    pub(super) scope: String,
    /// The conversation the row is filed under: the one contamination was
    /// cleared *for*. For the rotation path that is the OUTGOING session, not
    /// the new one — the new one was never contaminated, and filing it there
    /// would break a join against the `contamination` row that opened the
    /// lifecycle.
    pub(super) session: Option<String>,
    pub(super) root: String,
    pub(super) detail: String,
}

impl ContaminationCleared {
    /// Write the row. Fire-and-forget, like every other `record_flag` call on
    /// these paths.
    fn record(self) {
        warn!(
            target: "offload",
            consumer = self.consumer,
            scope = %self.scope,
            basis = self.basis.tool(),
            origin = self.origin.as_str(),
            root = %self.root,
            "loopback: V32 contamination flag cleared on the user's authority"
        );
        outbound::record_flag(outbound::Flag {
            screen: outbound::Screen::ContaminationCleared,
            origin: self.origin,
            consumer: self.consumer,
            scope: &self.scope,
            // Derived from the label this row was built with — see the
            // contamination row above (#48 F-29).
            attribution: outbound::scope_attribution(&self.scope),
            session: self.session.as_deref(),
            tool: self.basis.tool(),
            host: None,
            url: None,
            resolved_ip: None,
            canary: false,
            root: self.root.clone(),
            detail: &self.detail,
        });
    }

    /// Record a clear that may not have happened. Every `observe` call site
    /// funnels through this, so "the arm fired here" needs no branch of its own
    /// at five sites.
    fn record_from(cleared: Option<ClearedOnRotation>, scope: &LatchScope) {
        if let Some(ev) = cleared {
            ev.into_row(scope).record();
        }
    }
}

/// The sentence an incident reviewer reads when the bit was released —
/// composed **once**, for both paths.
///
/// Written as one function rather than two format strings because the whole
/// point of the pair is that they say the same things about different bases: the
/// prior state, what was and was not restored by the clear, and — the part a
/// reviewer most needs — that quarantined notes are untouched by it.
pub(super) fn clear_detail(
    basis: ClearBasis,
    origin: outbound::Origin,
    prior_latch: &str,
    prior_session: Option<&str>,
    new_session: Option<&str>,
) -> String {
    let how = match basis {
        ClearBasis::Resume => "the user judged the flagged content harmless and cleared the flag \
                               from the taint popover. The session, the tab and the working tree \
                               were not touched"
            .to_string(),
        ClearBasis::Unlatch => "the user restored FULL access from the taint popover (`unlatch`), \
                                and decision 15's 2026-08-10 amendment releases the flag with it: \
                                that click already hands back the strictly more dangerous \
                                capability — read AND web with the injected content still in the \
                                context window — so quarantining persistent memory afterwards \
                                would overrule the judgement it just asked for. An attacker \
                                cannot click it; the trust root is authority, not evidence. The \
                                session, the tab and the working tree were not touched"
            .to_string(),
        ClearBasis::Restore => format!(
            "the user restored a checkpoint, which armed a ONE-SHOT clear (a restore rolls back \
             files and cannot remove injected text from a context window, so the flag was kept), \
             and cImp has now observed this tab start a new harness session{}. The arm is the \
             authority here; the rotation only answers \"has it happened yet\", which is why a \
             forgeable rotation signal is acceptable for it and for nothing else",
            match new_session {
                Some(s) => format!(" ({s})"),
                None => String::new(),
            }
        ),
    };
    // The one sentence the three bases cannot share: two of them leave the latch
    // exactly where it was, and the third IS a latch move.
    let latch_note = match basis {
        ClearBasis::Unlatch => "The same click also moved the latch to `open` — that is what \
                                released the flag, and it is recorded as its own `latch_override` \
                                row.",
        ClearBasis::Resume | ClearBasis::Restore => {
            "The latch itself is unchanged by this and keeps its own controls."
        }
    };
    format!(
        "CONTAMINATION CLEARED (basis: {}, origin: {}): {how}. Prior state: contaminated=true, \
         latch={prior_latch}, session={}. {latch_note} Memory notes already quarantined STAY \
         quarantined — promoting or discarding them is the Memory view's own review (locked \
         decision 10), a separate consent surface. What changes is that this tab's future \
         persistent writes are stored clean again, and that a fresh contamination will report \
         itself as a new transition.",
        basis.tool(),
        origin.as_str(),
        prior_session.unwrap_or("unknown"),
    )
}

impl ClearedOnRotation {
    /// Turn a lock-side clear into the row its caller's scope can file.
    fn into_row(self, scope: &LatchScope) -> ContaminationCleared {
        debug_assert!(self.armed, "only an armed tab can clear on a rotation");
        ContaminationCleared {
            // NOT `Ipc`: see the field's doc. The click that authorised this
            // happened earlier and was recorded then.
            origin: outbound::Origin::Internal,
            basis: ClearBasis::Restore,
            consumer: scope.agent,
            scope: scope.label(),
            session: Some(self.prior_session.clone()),
            root: scope.root.clone(),
            detail: clear_detail(
                ClearBasis::Restore,
                outbound::Origin::Internal,
                self.prior_latch,
                Some(&self.prior_session),
                Some(&self.session),
            ),
        }
    }
}

/// **The one place a conversation is marked contaminated** (#48, finding F-3).
///
/// Both paths that can set the bit — an admitted proxied EXTERNAL call in
/// [`LatchRegistry::gate`], and the native-web beacon in
/// [`LatchRegistry::beacon`] — flip it *here*, so the transition cannot be set
/// on one path and recorded on the other, and a third path added later gets the
/// row by calling the only function that sets the bit.
///
/// # What it records, and what it deliberately does not
///
/// It records the **transition**, false → true, not every contaminating call.
/// Later EXTERNAL calls restate a fact this row already carries, and each
/// already writes an ordinary proxied-MCP activity row of its own; what had no
/// record at all was the moment the conversation stopped being clean. The
/// `mem::replace` below *is* the claim, so this is self-limiting and needs no
/// separate claim bit of the [`TabLatch::latch_flagged`] kind.
///
/// Because the bit is sticky across session rotations (H-2 — see
/// [`TabLatch::contaminated`]), "once" here means **once per tab**, not once
/// per conversation: a `/clear` in a contaminated tab keeps the taint and
/// writes no second row, and the row's `session` therefore names the
/// conversation contamination started in. A consumer joining these rows to
/// conversation-scoped state has to read them that way.
///
/// It does **not** decide whether the call contaminates. That is the caller's
/// classification (`gate` calls it only for `ToolClass::External` on a route
/// where EXTERNAL means content; the beacon calls it unconditionally, because a
/// beacon *is* the harness reporting that it read a page). Nothing about when
/// contamination is SET changes here — this is observability over an unchanged
/// decision.
///
/// # Why the return value must be used
///
/// The detection happens under the registry mutex and the write happens after
/// it is released, so the two are necessarily separate statements. `#[must_use]`
/// is what keeps them from drifting apart: a path that flips the bit and drops
/// the result fails the build under `-D warnings`, which is the same
/// "compile-time or it will be forgotten" posture `declare_screens!` takes for
/// the retention lane.
#[must_use = "a contamination transition that is detected and not recorded is finding F-3 again — \
              call Contamination::record() after dropping the registry lock"]
pub(super) fn note_contamination(
    entry: &mut TabLatch,
    scope: &LatchScope,
    tool: &str,
    prov: CallProvenance<'_>,
) -> Option<Contamination> {
    if std::mem::replace(&mut entry.contaminated, true) {
        return None;
    }
    Some(Contamination {
        origin: prov.origin,
        consumer: scope.agent,
        scope: scope.label(),
        // The registry entry's session, not the scope's: `observe` has already
        // run by the time any caller reaches here, so this is the session the
        // latch itself considers current — the one a later join has to match.
        session: entry.session.clone(),
        tool: tool.to_string(),
        url: prov.url.map(str::to_string),
        host: prov.host.map(str::to_string),
        root: scope.root.clone(),
        detail: format!(
            "CONTAMINATED: external content entered this conversation via {tool}{}. Nothing was \
             refused — the call was admitted, and this row records the state change it caused. \
             From here on every persistent memory write from this tab is quarantined for review \
             and every external result keeps its spotlighting envelope (latch={}). No \
             filesystem-derived signal clears the bit (H-2: a new harness session is not proof of \
             a new context window, because the model's own shell can forge one) and no HTTP route \
             can. It is cleared only by the USER, from the taint popover — immediately \
             (`clear_contamination`, \"that content was harmless\"), by restoring FULL access \
             (`unlatch`, which accepts the larger risk deliberately), or after a checkpoint \
             restore (`await_session_clear`, effective once cImp observes a new harness session). \
             The workflow flip (`flip_local`) does NOT clear it. Whichever happens, it writes its \
             own `contamination_cleared` row.",
            match prov.host {
                Some(h) => format!(" from {h}"),
                None => String::new(),
            },
            entry.latch.label(),
        ),
    })
}

/// #48 (2026-08-08 re-review), finding M-19 — what [`LatchRegistry::gate`]
/// hands back to a caller it could not attribute to a tab.
///
/// # The asymmetry this closes
///
/// The identity-less fail-open is locked (F-5/H-8) and load-bearing: a child
/// spawned before `--tab` existed, and the documented headless consumers, must
/// keep their TOOL-SERVING routes. Nothing here touches that — every class
/// except one still leaves this function `Clean`, and no latch row is created
/// for any of them.
///
/// PERSISTENT-WRITE is the exception, and the precedent for treating it as one
/// is already in this codebase, one module over: on the **headless** path a
/// write with no identity is refused outright (`graph::mcp::headless_refusal`,
/// `HEADLESS_WRITE_UNAVAILABLE`), on exactly this reasoning — that path has
/// neither a session identity nor a taint verdict, and a note written blind
/// with neither is *"project-wide, permanent, unattributable AND unquarantined,
/// which is the highest-privilege write the memory surface offers"*. The
/// loopback path reached the identical state and stored the note clean. Two
/// paths, the same two missing facts, opposite answers.
///
/// # Why quarantine and not the headless path's refusal
///
/// Locked decision 10. A refusal on this path throws away the legitimate
/// research conclusion the session existed to produce; the quarantine keeps it,
/// flags it, hides it from `context_recall` / `context_notes` / compaction
/// carry-over / the fact distiller, and hands the user promote-or-discard. (The
/// headless path refuses because there is no running app to review a queue in,
/// not because refusal is the better answer.)
///
/// It is [`WriteTaint::Unattributed`] rather than `Quarantined` so the model is
/// told the actual reason — see that variant.
///
/// # Deliberately not gated on `policy.latch`
///
/// Only on `policy.quarantine`, matching the scoped path: locked decision 16
/// keeps the two switches independent, and this is a quarantine decision, not a
/// latch decision. Nothing here reads or moves a latch — there is no scope to
/// move one for, which is the whole point.
///
/// #48 F-16: `prov` is here for one field — [`CallProvenance::root`]. The
/// finding's own wording, *"`LatchRegistry::gate` has no scope to derive a root
/// from"*, is true of `gate` and **false of the route that calls it**: the only
/// route that can reach this line with a PERSISTENT-WRITE is `/graph_run`, and
/// that handler holds the project the note is about to be written into.
pub(super) fn unattributed_write(
    policy: GatePolicy,
    route: LatchRoute,
    name: &str,
    prov: CallProvenance<'_>,
) -> WriteTaint {
    let class = toolclass::classify(name);
    if !policy.quarantine || class != ToolClass::PersistentWrite || !route.can_execute(name, class)
    {
        return WriteTaint::Clean;
    }
    warn!(
        target: "offload",
        tool = %name,
        "loopback: persistent memory write held — the caller carries no resolvable tab identity"
    );
    // One row per held note, for the same reason the scoped quarantine writes
    // one: each is a separate item in the user's review queue, and a feed that
    // reported only the first would leave later ones discoverable solely by
    // opening the Memory view. There is no scope, so the columns that name one
    // say so rather than guessing — the `consumer` a request body could have
    // supplied is exactly the field M-19 showed is caller-chosen.
    outbound::record_flag(outbound::Flag {
        screen: outbound::Screen::MemoryQuarantine,
        origin: outbound::Origin::Internal,
        consumer: "unattributed",
        scope: "unattributed",
        // #48 F-29 — the reason this field exists. `"unattributed"` is a
        // description, not a scope, and the old derivation (anything without a
        // `:` ⇒ `Headless`) turned it into the positive claim *"a run with no tab
        // behind it"*. This frame does not know that: it knows only that the
        // caller's tab identity did not resolve, which is what
        // `Attribution::Unattributed` says and what this whole row is about.
        //
        // Not more precise than that ON PURPOSE. The route's `LatchScoping`
        // could distinguish "no id was sent" (`Headless`) from "an id that names
        // no configured tab" (`Unrecognized`), but `gate` receives
        // `Option<&LatchScope>` and both collapse to `None` before this line —
        // the same collapse F-20 fixed one seam over. Recovering it means
        // threading the attribution through `CallProvenance`, a separate change;
        // claiming either half from here would be a guess.
        attribution: crate::activity::Attribution::Unattributed,
        session: None,
        tool: name,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        // #48 F-16: the route's project, not an empty string. There is no scope
        // to take a root from — that is this function's whole premise — but the
        // ROUTE knows which project the note is about to be written into, and
        // that is the project a reviewer filters by. `None` would be a route with
        // no project in view, which cannot reach this line today; the fallback is
        // still empty rather than invented, and `""` is a positive claim of
        // ignorance with a documented meaning (see `ActivityEntry::root`).
        root: prov.root.unwrap_or_default().to_string(),
        detail: toolclass::UNATTRIBUTED_WRITE_NOTICE,
    });
    WriteTaint::Unattributed
}

/// #48 (F-34) — pick the refusal that states the cause the gate **checked**,
/// for the one latch position that has two possible causes.
///
/// # What this is, and what it is deliberately not
///
/// It is a **message selector**, not a gate. Containment is decided entirely by
/// [`Latch::proxy_gate`](toolclass::Latch::proxy_gate) before this runs and is
/// byte-identical to what it always was: `Some(_)` in, `Some(_)` out, and the
/// same calls refused. `local_by_user_flip` never joins the guard — for F-13's
/// reason, an unknown value must be able to cost only the better *message* and
/// never the refusal, so a `false` here simply serves the pre-F-34 constant.
///
/// # Why it is here and not in [`Latch::refusal`](toolclass::Latch::refusal)
///
/// **Locked decision 34 places the choice in `LatchRegistry::gate`, which holds
/// `TabLatch::local_by_user_flip`, and NEVER in `Latch::refusal`** — and that is
/// load-bearing rather than stylistic. `Latch::refusal` is a *pure function over
/// [`Latch`](toolclass::Latch)* that the **offload worker** also calls
/// (`offload::agent`), and the worker has no user-flip concept to thread:
/// migrating this down would either break it or force it to pass a meaningless
/// `false` forever. The rule is a convention, not a type, so it is also guarded
/// by a tripwire in `toolclass`
/// (`the_user_flip_constant_is_never_reachable_from_the_pure_latch_functions`).
///
/// Written as a free function taking the bool rather than a `TabLatch` method so
/// the ONE fact it may consult is visible in its signature: nothing from the
/// caller, the model or the tool arguments can reach the string.
///
/// # The match, and why on the constant
///
/// `REFUSAL_EXTERNAL_BLOCKED` is produced by exactly one state — `Latch::Local`
/// blocking [`ToolClass::External`] — so keying on it is equivalent to keying on
/// that pair, and it cannot silently capture a future refusal that means
/// something else. The other two constants are unreachable under a `Local`
/// latch, so they fall through untouched.
pub(super) fn user_flip_refusal(refusal: &'static str, local_by_user_flip: bool) -> &'static str {
    if local_by_user_flip && refusal == toolclass::REFUSAL_EXTERNAL_BLOCKED {
        // The user's own IPC flip closed the external side. Saying a tool call
        // did it is F-23's defect on the route that ships ON.
        toolclass::REFUSAL_EXTERNAL_USER_LOCAL
    } else {
        refusal
    }
}

impl LatchRegistry {
    /// Decide one call, and engage the latch when it may proceed.
    ///
    /// The whole check-then-engage runs under one lock and **before** the call
    /// executes — loopback serves concurrent requests, so two simultaneous
    /// calls from one tab must not both observe an open latch. A refused call
    /// never engages or flips anything (same property as Phase A's
    /// `latch_gate`): otherwise a hallucinated call to the blocked side could
    /// redefine which side of the boundary the session is on.
    ///
    /// V32 Phase C2: the success arm now carries a [`WriteTaint`]. It is
    /// [`WriteTaint::Quarantined`] for exactly one case — a PERSISTENT-WRITE
    /// under an EXTERNAL latch, which Phase B refused and locked decision 10
    /// turns into a quarantined write — and `Clean` for everything else. The
    /// caller must thread it into the call it is about to make; ignoring it
    /// would store an externally-influenced note as ordinary memory.
    ///
    /// V32 Phase G: `policy` carries the two feature switches (locked decision
    /// 16). With both off the gate returns immediately without touching any
    /// state — a disabled control must leave no trace, not merely no verdict,
    /// or `/status` would keep showing latches the user turned off.
    ///
    /// #48 (F-3): `prov` is what the calling route knows and the registry
    /// cannot derive — see [`CallProvenance`]. It is used for exactly one thing
    /// here: the [`Screen::Contamination`](outbound::Screen) row, written when
    /// an admitted call is the one that stops this conversation being clean.
    pub(super) fn gate(
        &self,
        scope: Option<&LatchScope>,
        route: LatchRoute,
        name: &str,
        policy: GatePolicy,
        prov: CallProvenance<'_>,
    ) -> Result<WriteTaint, &'static str> {
        if policy.inert() {
            return Ok(WriteTaint::Clean);
        }
        // Fail-open: no tab identity ⇒ no latch (see [`latch_scope`]) — except
        // for the one class where "we do not know who this is" is itself the
        // hazard. See [`unattributed_write`].
        let Some(scope) = scope else {
            return Ok(unattributed_write(policy, route, name, prov));
        };
        let class = toolclass::classify(name);
        // #48, findings A-1 and M-2: a call that cannot execute on this route
        // is not evidence of anything, so it neither latches nor is refused —
        // see [`LatchRoute::can_execute`] for both rules and why this does not
        // weaken unknown-⇒-EXTERNAL.
        if !route.can_execute(name, class) {
            return Ok(WriteTaint::Clean);
        }
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tabs.entry(scope.key()).or_insert_with(TabLatch::fresh);
        // Step 4: for a Claude tab this is the usual place an armed one-shot
        // fires — the first cImp tool call of the conversation that followed the
        // user's `/clear`. Recorded on every exit below, before this call's own
        // rows, so the feed reads in the order the state actually moved.
        let rotated = entry.observe(scope.session.as_deref());
        // V32 Phase F: quarantine keys on CONTAMINATION, not on the latch
        // position. `Latch::External` always implies `contaminated` (both are
        // set by the same admitted call, a few lines below), so this only ever
        // WIDENS the pure-latch verdict `proxy_gate` computes — to the one case
        // decision 15 creates, where a user override moved the latch off
        // External on a conversation that has already read external content.
        // The pure function stays the single definition of the latch's own
        // semantics; the bit is layered over it here, at the only site that
        // owns per-conversation state.
        //
        // V32 Phase G layers the two switches over the same expression, and the
        // order is the whole point: the latch's verdict is computed only when
        // the latch feature is on, and the quarantine verdict only when the
        // quarantine feature is on. So "latch off, quarantine on" still holds a
        // note written after a fetch (contamination is tracked below regardless
        // of the latch), and "latch on, quarantine off" refuses the same calls
        // it always did while storing writes clean.
        let latched = if policy.latch {
            entry.latch.proxy_gate(class)
        } else {
            ProxyGate::Proceed(WriteTaint::Clean)
        };
        let decision = match latched {
            ProxyGate::Proceed(WriteTaint::Clean)
                if policy.quarantine
                    && class == ToolClass::PersistentWrite
                    && entry.contaminated =>
            {
                ProxyGate::Proceed(WriteTaint::Quarantined)
            }
            ProxyGate::Proceed(WriteTaint::Quarantined | WriteTaint::Unattributed)
                if !policy.quarantine =>
            {
                ProxyGate::Proceed(WriteTaint::Clean)
            }
            other => other,
        };
        let refusal = match decision {
            // `Unattributed` cannot arrive here — it is [`unattributed_write`]'s
            // answer, returned above this frame's `scope` binding, and
            // `proxy_gate` never produces it. It is bound rather than excluded
            // so that a future path which does route one through holds the note
            // (and explains it correctly) instead of falling into the `Clean`
            // arm below.
            ProxyGate::Proceed(held @ (WriteTaint::Quarantined | WriteTaint::Unattributed)) => {
                // Locked decision 10: store it, flag it, hold it for the user.
                // The write itself never latches (PERSISTENT-WRITE is not a
                // latching class), so nothing about the scope changes here.
                warn!(
                    target: "offload",
                    agent = scope.agent,
                    tab = %scope.tab,
                    tool = %name,
                    latch = entry.latch.label(),
                    "loopback: persistent memory write quarantined by the V32 session taint latch"
                );
                // Unlike the refusal below this is NOT one-row-per-scope: each
                // quarantined note is a separate item in the user's review
                // queue, and a feed that reported only the first would leave
                // later ones discoverable solely by opening the Memory view.
                drop(tabs);
                ContaminationCleared::record_from(rotated, scope);
                outbound::record_flag(outbound::Flag {
                    screen: outbound::Screen::MemoryQuarantine,
                    origin: outbound::Origin::Internal,
                    consumer: scope.agent,
                    scope: &scope.label(),
                    // #48 F-29: the scope is in hand, so the tab id comes from
                    // its field rather than from a re-split label.
                    attribution: scope.attribution(),
                    session: scope.session.as_deref(),
                    tool: name,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: scope.root.clone(),
                    detail: held
                        .write_notice()
                        .unwrap_or(toolclass::QUARANTINE_WRITE_NOTICE),
                });
                return Ok(held);
            }
            ProxyGate::Proceed(WriteTaint::Clean) => None,
            // #48 (F-34): the message, and ONLY the message. `decision` above is
            // untouched, so what gets refused is byte-identical to what always
            // did — this arm just picks which fixed constant states the cause,
            // from a fact only this frame holds. See [`user_flip_refusal`].
            ProxyGate::Refuse(r) => Some(user_flip_refusal(r, entry.local_by_user_flip)),
        };
        if let Some(refusal) = refusal {
            warn!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %name,
                latch = entry.latch.label(),
                "loopback: tool call refused by the V32 session taint latch"
            );
            // V32 Phase C: Phase B left this refusal without a consumer — the
            // user could only see it as a tool that mysteriously stopped
            // working. One row per scope (see `TabLatch::latch_flagged`).
            let first = !std::mem::replace(&mut entry.latch_flagged, true);
            drop(tabs);
            ContaminationCleared::record_from(rotated, scope);
            if first {
                outbound::record_flag(outbound::Flag {
                    screen: outbound::Screen::LatchRefusal,
                    origin: outbound::Origin::Internal,
                    consumer: scope.agent,
                    scope: &scope.label(),
                    // The scope is in hand — see `LatchScope::attribution`.
                    attribution: scope.attribution(),
                    session: scope.session.as_deref(),
                    tool: name,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: scope.root.clone(),
                    detail: refusal,
                });
            }
            return Err(refusal);
        }
        // V32 Phase F: the call is admitted, so if it is EXTERNAL its content is
        // about to enter this conversation. Set the contamination bit HERE
        // rather than deriving it from the latch, because the latch is now
        // user-movable and the bit is not. (A refused call never reaches this
        // point, so a hallucinated call to the blocked side cannot contaminate
        // a clean session — the same property `engage` has.)
        //
        // V32 Phase G: tracked whenever EITHER switch is on (an inert policy
        // returned above), because contamination is the quarantine's input as
        // much as the latch's — a user who keeps quarantine but drops the latch
        // still needs "this conversation read a page" to be true.
        //
        // #48 (F-3): and the TRANSITION is recorded, through the one function
        // that owns the bit. This was the finding's whole substance — the line
        // this replaces set the bit silently, and the only trace was the `info!`
        // below, which fires on the *latch* transition. A tab already latched
        // `Local`, or one running with the latch feature off and the quarantine
        // on, contaminated with no timestamp, no tool and no row. The condition
        // is unchanged, deliberately: recording must follow the same rule the
        // bit does, or the switch combination that made it silent still would.
        //
        // Engagement is the LATCH's own state, so it moves only while the latch
        // feature is on: a latch shown as engaged in `/status` while the feature
        // is off would describe a boundary that is not being enforced. It is
        // sequenced ahead of the contamination note for one reporting reason —
        // the row quotes the latch this call leaves the tab in, so a fresh tab's
        // contamination row must say `external` rather than the `open` it was a
        // microsecond earlier. Nothing can observe the entry between the two
        // (both run under the one lock), so the order is a choice about the row,
        // not about the semantics.
        //
        // #48 (M-7): …and only on a route whose calls are ELECTIVE. A
        // [`LatchRoute::Hook`] call is cImp's own automation firing over work
        // the harness already permitted, so it reads the latch (it can be
        // refused, three lines up) and never moves it. `engages()` is checked
        // here rather than inside `Latch::engage` because it is a fact about
        // the route, not about the class.
        let engaged = policy.latch && route.engages() && entry.latch.engage(class);
        let contamination = if class == ToolClass::External {
            note_contamination(entry, scope, name, prov)
        } else {
            None
        };
        let latch = entry.latch.label();
        // Both the log line and the row are written with the lock released —
        // `record_flag` reaches the activity store, which does file I/O.
        // (Step 4's clear row is written first, below, for the same ordering
        // reason the beacon states.)
        drop(tabs);
        ContaminationCleared::record_from(rotated, scope);
        if engaged {
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %name,
                latch,
                "loopback: V32 session taint latch engaged"
            );
        }
        if let Some(contamination) = contamination {
            contamination.record();
        }
        Ok(WriteTaint::Clean)
    }

    /// V32 Phase F (locked decision 14): engage this tab's EXTERNAL latch on
    /// behalf of a HARNESS-NATIVE web tool that never routed through cImp.
    ///
    /// The beacon is the sensor mode's whole mechanism: Claude's `WebFetch` /
    /// `WebSearch` and OpenCode's `webfetch` / `websearch` bypass the proxy, so
    /// without this a tab could read an attacker's page while `/status` still
    /// says `open` and every proxied local-capability tool stays available
    /// beside it. It does exactly what an admitted proxied EXTERNAL call does —
    /// engage the latch, set the contamination bit — and deliberately nothing
    /// more:
    ///
    /// - **No refusal, ever.** The tool has already been permitted by the
    ///   harness by the time the hook runs (and in `deny` mode it never runs at
    ///   all). Returning "blocked" here would be a lie the caller cannot act on.
    /// - **Fail-open on identity**, like every other gate here: a beacon with no
    ///   tab id — or, since #45, with an id that is not a configured tab — has
    ///   no scope to engage.
    ///
    /// It reports what changed ([`BeaconOutcome`]) rather than writing the row
    /// itself: the row's honesty depends on the [`outbound::Origin`] of the
    /// request that caused it, and the registry cannot see that. The handler
    /// owns it (#45).
    ///
    /// The one asymmetry with `gate`: a beacon arriving while the tab is
    /// LOCAL-latched cannot refuse the fetch (it already happened), so it
    /// records the contamination and leaves the latch where it is — the honest
    /// reading of "this conversation has now seen external content, and its
    /// proxied external side stays closed". That case is exactly the one #45
    /// left unaudited; see [`BeaconOutcome::contaminated_now`] (#48).
    ///
    /// V32 Phase G: gated by the same [`GatePolicy`] a proxied call resolves.
    /// An inert policy answers with the default view and records nothing — a
    /// beacon whose latch and quarantine are both off has nothing to report to.
    ///
    /// #48 (F-3): the [`Screen::LatchBeacon`](outbound::Screen) row the handler
    /// writes is unchanged and still says what it always said — *a native web
    /// tool was detected*. The contamination row this method now writes says
    /// something different — *this conversation stopped being clean* — and a
    /// beacon into an already-contaminated tab writes only the first. `prov`
    /// carries the [`outbound::Origin`] for the same reason the handler states
    /// it for the beacon row: over this route it is `Http`, and that is a fact
    /// about the caller, not about the tab.
    pub(super) fn beacon(
        &self,
        scope: Option<&LatchScope>,
        tool: &str,
        policy: GatePolicy,
        prov: CallProvenance<'_>,
    ) -> BeaconOutcome {
        if policy.inert() {
            return BeaconOutcome::inert();
        }
        let Some(scope) = scope else {
            return BeaconOutcome::inert();
        };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tabs.entry(scope.key()).or_insert_with(TabLatch::fresh);
        let cleared = entry.observe(scope.session.as_deref());
        // Unchanged, deliberately: contamination is set on every beacon, and
        // nothing on THIS route can ever clear it (locked decision 15 — step 4's
        // two clears are user actions over IPC, and `observe` above releases the
        // bit only for a tab the user armed). What #48
        // adds is only that the TRANSITION is observable, so it can be recorded
        // — through the SAME function the proxied gate flips the bit with, so
        // the two paths cannot disagree about what a transition is or produce
        // two shapes of row for it. Ordered after the engagement for the same
        // reporting reason `gate` states: the row quotes the latch this beacon
        // leaves the tab in.
        let moved = policy.latch && entry.latch.engage(ToolClass::External);
        let contamination = note_contamination(entry, scope, tool, prov);
        let contaminated_now = contamination.is_some();
        // One row per tab-session over BOTH transitions, rather than one per
        // transition kind: a policy change mid-session could otherwise produce a
        // second row for the same conversation.
        let report =
            (moved || contaminated_now) && !std::mem::replace(&mut entry.beacon_flagged, true);
        let view = entry.view();
        drop(tabs);
        // Step 4: recorded BEFORE the contamination row this beacon may also
        // produce. A beacon arriving on the first call after an armed rotation
        // clears the bit and immediately re-sets it, and the feed has to read in
        // that order to make sense.
        ContaminationCleared::record_from(cleared, scope);
        if moved {
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                latch = view.latch,
                "loopback: V32 session taint latch engaged by a native-web beacon"
            );
        } else if contaminated_now {
            // The case #45 left entirely silent: the latch did not move (the tab
            // is latched `Local`, or the latch feature is off) but this
            // conversation is contaminated from here on.
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                latch = view.latch,
                "loopback: V32 conversation marked contaminated by a native-web beacon (latch unmoved)"
            );
        }
        if let Some(contamination) = contamination {
            contamination.record();
        }
        BeaconOutcome {
            view,
            engaged: moved,
            contaminated_now,
            report,
        }
    }

    /// V32 Phase H: this tab's current view, **read-only** — the state the
    /// OpenCode plugin's native-tool gate decides against.
    ///
    /// Two properties, both deliberate:
    ///
    /// - **It does not create an entry.** A tab that has never made a gated call
    ///   has nothing to report, and materializing a row for every poll would put
    ///   tabs in `/status` that no tool call ever touched. Absent ⇒
    ///   [`LatchView::default`] ⇒ `open` ⇒ the gate denies nothing. Fail-open by
    ///   construction, not by a branch someone has to remember.
    /// - **It DOES `observe`.** A stale `external` left over from a rotated
    ///   session would deny `read`/`bash` for a whole fresh conversation — a
    ///   false deny of the harness's core tools, which is far worse than the
    ///   read-only purity of not touching the entry. `observe` is the same
    ///   rotation rule `gate` and `beacon` apply, so the three cannot disagree
    ///   about when a conversation ended.
    ///
    /// Step 4: which also means this is one of the places an armed one-shot
    /// fires. For an OpenCode tab it is the *usual* one — the plugin polls
    /// `/latch/state` around the harness's own turns, so a `/clear` after a
    /// restore lifts the bit without waiting for a proxied tool call.
    pub(super) fn view_for(&self, scope: &LatchScope) -> LatchView {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let (view, cleared) = match tabs.get_mut(&scope.key()) {
            Some(entry) => {
                let cleared = entry.observe(scope.session.as_deref());
                (entry.view(), cleared)
            }
            None => (LatchView::default(), None),
        };
        drop(tabs);
        ContaminationCleared::record_from(cleared, scope);
        view
    }

    /// V32 Phase F (locked decision 15): apply a USER-initiated latch move.
    ///
    /// Decision 1 rejected automatic resets and still does — an injected context
    /// stays injected, so the latch never *releases itself*. What decision 15
    /// adds is a human, who knows something the system cannot infer: that the
    /// research is done and its output has been read.
    ///
    /// **What enforces "a human" (#45).** This used to claim "nothing the model
    /// can reach may move this", which was false: the same implementation was
    /// also exposed as `POST /latch/override`, behind nothing but the per-launch
    /// bearer token — and that token is readable by any process running as the
    /// user (`.cimp-offload.json`, `.cimp-discovery/<pid>.json`, and the
    /// generated OpenCode plugin inside the project tree). That route is GONE.
    /// What the code now enforces, exactly:
    ///
    /// - **The only caller is the capability-scoped `latch_override` Tauri IPC
    ///   command**, driven by the badge popover. The webview holds no bearer
    ///   token and makes no HTTP call, so this path is not reachable from
    ///   outside the app process.
    /// - **What a shell-capable model CAN still reach is `/latch/beacon`**,
    ///   which only ever tightens (Open → External) and only for a configured
    ///   tab id ([`is_configured_tab`]) — it cannot flip to Local, cannot
    ///   unlatch, and cannot clear contamination.
    /// - **What clears `contaminated`** is three of the four actions —
    ///   `clear_contamination`, `unlatch` (decision 15's 2026-08-10 amendment)
    ///   and, deferred, `await_session_clear` — and nothing else: no automatic
    ///   path, no HTTP path. `flip_local` is the one that does not. See
    ///   [`TabLatch::contaminated`] for why a click is a legitimate trust root
    ///   where a transcript file is not.
    ///
    /// This is not an integrity boundary against native code, and never was —
    /// decision 3 says plainly that a model with a shell already has the
    /// capabilities the latch withholds. It is the difference between an audit
    /// trail that records a user's decision and one that records a POST.
    ///
    /// **The feature switches are deliberately not consulted.** `gate` creates
    /// no entry while [`GatePolicy::inert`], so with both controls off there is
    /// usually nothing here to move and the caller gets the "nothing to
    /// override" error. But an entry created while the controls were ON survives
    /// the user switching them off, and its contamination bit is still what the
    /// badge renders — so refusing to clear it would leave a stale flag the user
    /// cannot reach *because* they disabled the feature. Every action here is
    /// user-initiated and only ever loosens cImp's own bookkeeping; none of them
    /// needs the feature to be armed to be meaningful.
    ///
    /// Errors (rather than silently no-op'ing) when the move does not apply, so
    /// the UI can say why instead of appearing to have worked.
    pub(super) fn apply_override(
        &self,
        scope: &LatchScope,
        action: LatchOverride,
    ) -> Result<OverrideOutcome, String> {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = tabs.get_mut(&scope.key()) else {
            return Err(format!(
                "no taint latch is engaged for {} — nothing to override",
                scope.label()
            ));
        };
        // An armed one-shot can fire right here: the user restored, ran
        // `/clear`, and the first thing to look at the entry afterwards is their
        // next click. Captured rather than dropped, and recorded below on BOTH
        // exits — a refused action must not swallow a clear that already
        // happened.
        let rotated = entry.observe(scope.session.as_deref());
        let prior = entry.latch;
        let mut prior_taint = None;
        // The action's own verdict, computed before the lock is released so an
        // error path can still record `rotated`.
        let applied = match action {
            // The workflow button: research finished, now apply it. EXTERNAL
            // only — from `Open` there is nothing to flip and from `Local` this
            // would be a no-op that reads like an action. At no instant does the
            // session hold web AND local capability: the flip closes the
            // external side in the same assignment that opens the local one.
            LatchOverride::FlipLocal => {
                if prior != Latch::External {
                    Err(format!(
                        "\"switch to local\" applies only to an EXTERNAL-latched tab ({} is {})",
                        scope.label(),
                        prior.label()
                    ))
                } else {
                    entry.latch = Latch::Local;
                    // #48 (F-23): record WHY the latch is `local`, at the only
                    // site that can know. This is the fact the native-web
                    // refusal is selected on — see
                    // [`TabLatch::local_by_user_flip`] — and it is written under
                    // the same lock as the assignment above so the two cannot be
                    // observed apart.
                    entry.local_by_user_flip = true;
                    Ok(())
                }
            }
            // The at-own-risk button: both sides open again. Valid from any
            // state except a latch that is already open, which would be a
            // no-op.
            //
            // Decision 15's 2026-08-10 amendment: **it also clears the
            // contamination bit.** The trust root is the one that closed H-2 —
            // authority, not evidence. An attacker cannot click this; the click
            // already hands back the strictly more dangerous capability (read +
            // web, with the injected content still in the context window) behind
            // the popover's own confirmation; so leaving persistent memory
            // quarantined afterwards overruled a judgement the product had just
            // asked the user to make. `FlipLocal` above keeps the bit precisely
            // because it is a workflow step and not a verdict.
            //
            // Ordering is load-bearing: the clear runs BEFORE `latch = Open`, so
            // `PriorTaint::latch` records the latch the bit was released from
            // (`external`/`local`) rather than the `open` this arm is about to
            // write — which is what keeps it equal to `OverrideOutcome::prior`,
            // the value `override_row` puts in the same sentence.
            LatchOverride::Unlatch => {
                if prior == Latch::Open {
                    Err(format!(
                        "{} is not latched — nothing to unlatch",
                        scope.label()
                    ))
                } else {
                    // `None` here is not an error: the unlatch is legal on its
                    // own terms and the clear is a consequence of it, not its
                    // purpose. An uncontaminated latched tab unlatches and
                    // writes no `contamination_cleared` row — see
                    // `unlatch_clear_row`.
                    prior_taint = entry.clear_contamination();
                    entry.latch = Latch::Open;
                    // #48 (F-23): the web side is open again, so nothing is being
                    // refused for this reason any more. Cleared here rather than
                    // left to the next rotation because the field must never
                    // outlive the latch position it explains.
                    entry.local_by_user_flip = false;
                    Ok(())
                }
            }
            // Step 4, the false-positive resume. Clears the bit and NOTHING
            // else: the latch stays where it is (it has its own buttons), the
            // budget keeps its spend, the session and the tab are not touched,
            // and quarantined notes stay quarantined.
            //
            // It supersedes an arm, which `clear_contamination` drops — there is
            // nothing left to wait for once the bit is gone.
            LatchOverride::ClearContamination => match entry.clear_contamination() {
                Some(p) => {
                    prior_taint = Some(p);
                    Ok(())
                }
                None => Err(format!(
                    "{} is not flagged as contaminated — nothing to clear",
                    scope.label()
                )),
            },
            // Step 4, the restore arm. Clears nothing now, by user decision: a
            // restore rolls back FILES and cannot remove injected text from the
            // model's context window, so this is the case where clearing
            // immediately is least justified.
            LatchOverride::AwaitSessionClear => {
                if !entry.contaminated {
                    Err(format!(
                        "{} is not flagged as contaminated — there is nothing waiting to clear",
                        scope.label()
                    ))
                } else if entry.awaiting_session_clear {
                    // Not a failure so much as an answer, and the popover shows
                    // it verbatim. Still an error rather than a silent success:
                    // a second click that reported "done" would imply something
                    // new happened.
                    Err(format!(
                        "{} is already waiting for a new session — the contamination flag clears \
                         when one is observed",
                        scope.label()
                    ))
                } else {
                    entry.awaiting_session_clear = true;
                    Ok(())
                }
            }
        };
        // Deliberately NOT touched by ANY move here: the session's spent budget.
        // Letting a click refill the fetch budget would make the budget
        // advisory. (Live-verified 2026-08-10: an unlatch does not refill it —
        // recipe 13's web-side leg could not be re-probed for exactly that
        // reason.)
        //
        // And `contaminated` is not touched by `FlipLocal`: the flip changes
        // what the session may reach next and cannot un-read what the model has
        // already read. `Unlatch` DOES release it — decision 15's 2026-08-10
        // amendment, argued in that arm — and `Unlatch` is the only latch move
        // that does.
        let view = entry.view();
        drop(tabs);
        ContaminationCleared::record_from(rotated, scope);
        applied?;
        warn!(
            target: "offload",
            agent = scope.agent,
            tab = %scope.tab,
            action = action.as_str(),
            prior = prior.label(),
            latch = view.latch,
            contaminated = view.contaminated,
            awaiting_session_clear = view.awaiting_session_clear,
            "loopback: V32 containment state moved by explicit user override"
        );
        Ok(OverrideOutcome {
            prior,
            prior_taint,
            view,
        })
    }

    /// Step 4: fold each known tab's CURRENT live session into its entry, so a
    /// rotation the harness has already proved reaches [`TabLatch::observe`]
    /// even when the tab has made no gated call since.
    ///
    /// **Why the read path needs this.** Before step 4, `observe` ran only from
    /// `gate`, `beacon` and `view_for` — i.e. only when the harness did
    /// something. That was fine when a rotation had no user-visible consequence
    /// worth waiting for. It is not fine now: the whole promise of the restore
    /// arm is "run `/clear` and the flag lifts", and a Claude tab has no
    /// `/latch/state` poll, so without this the flag would sit set until the
    /// model happened to call a cImp tool. This is the read the UI already makes
    /// every 4 s ([`latch_snapshot`]) — no second timer, no new schedule.
    ///
    /// **It grants nothing a call would not have granted anyway.** Everything
    /// `observe` resets is permissive state that the very next gated call would
    /// have reset before deciding anything, so doing it at read time is strictly
    /// a matter of *when* the same fact becomes visible.
    ///
    /// Takes resolved scopes rather than an `AppHandle` so the session lookup
    /// (which locks the graph service) happens outside this lock.
    pub(super) fn observe_all(&self, scopes: &[LatchScope]) -> Vec<ContaminationCleared> {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cleared = Vec::new();
        for scope in scopes {
            if let Some(entry) = tabs.get_mut(&scope.key()) {
                if let Some(ev) = entry.observe(scope.session.as_deref()) {
                    cleared.push(ev.into_row(scope));
                }
            }
        }
        cleared
    }

    /// Every `(agent, tab)` the registry holds an entry for. Cloned out under
    /// the lock so [`latch_snapshot`] can resolve live sessions without holding
    /// it.
    pub(super) fn keys(&self) -> Vec<(&'static str, String)> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        tabs.keys().cloned().collect()
    }

    /// V32 Phase C (locked decision 11): whether this tab's session may make
    /// another EXTERNAL call, and the one-per-scope exhaustion report.
    ///
    /// Runs only on `/mcp/call` — every name that route serves is proxied and
    /// therefore EXTERNAL; `/graph_run` serves cImp-native tools that pull no
    /// external bytes and are not budgeted. Fail-open on a call with no tab
    /// identity, exactly like [`gate`](Self::gate): there is no scope to charge.
    pub(super) fn budget_gate(
        &self,
        scope: Option<&LatchScope>,
        limits: outbound::BudgetLimits,
        tool: &str,
    ) -> Result<(), &'static str> {
        let Some(scope) = scope else { return Ok(()) };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = tabs.get_mut(&scope.key()) else {
            // No entry yet ⇒ `gate` has not run for this tab, so nothing is
            // spent. (In practice `gate` always runs first on this route.)
            return Ok(());
        };
        if !entry.budget.exhausted(limits) {
            return Ok(());
        }
        let first = entry.budget.claim_flag();
        drop(tabs);
        if first {
            warn!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                "loopback: external fetch budget exhausted for this session"
            );
            outbound::record_flag(outbound::Flag {
                screen: outbound::Screen::Budget,
                origin: outbound::Origin::Internal,
                consumer: scope.agent,
                scope: &scope.label(),
                // The scope is in hand — see `LatchScope::attribution`.
                attribution: scope.attribution(),
                session: scope.session.as_deref(),
                tool,
                host: None,
                url: None,
                resolved_ip: None,
                canary: false,
                root: scope.root.clone(),
                detail: outbound::REFUSAL_BUDGET,
            });
        }
        Err(outbound::REFUSAL_BUDGET)
    }

    /// Charge one completed EXTERNAL call to this tab's session budget.
    /// Silently no-ops without tab identity (nothing to charge) — the same
    /// fail-open the latch takes.
    pub(super) fn charge(&self, scope: Option<&LatchScope>, response_bytes: usize) {
        let Some(scope) = scope else { return };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tabs.get_mut(&scope.key()) {
            entry.budget.charge(response_bytes);
        }
    }

    /// Charge one **attempted** proxied call, whatever it returned (#48, D-3).
    ///
    /// The charge used to sit on the `Ok` arm only, so a loop of fetches
    /// against a host that 500s advanced neither the byte counter nor the call
    /// counter and never exhausted the budget — while the worker's copy of the
    /// same contract charged both arms (an `Err` there becomes an `ERROR: …`
    /// tool result with `executed = true`). Two paths, one contract, opposite
    /// behaviour.
    ///
    /// A failed fetch charges **zero bytes and one call**: nothing was
    /// ingested, but the request left the machine and `max_calls` is what
    /// exists to stop a loop. Taking the whole decision here — rather than a
    /// `map` at the call site — is what makes it testable: the handler's use is
    /// one unconditional statement above the match it used to be inside.
    /// Generic over the error half since #48 M-17 made it a
    /// `mcp_host::HostError`: this function reads only `is_ok`/`len`, and the byte
    /// charge is unchanged by the error type.
    pub(super) fn charge_call<E>(&self, scope: Option<&LatchScope>, result: &Result<String, E>) {
        self.charge(scope, result.as_ref().map(|t| t.len()).unwrap_or(0));
    }

    /// Claim one of this tab session's audit-row bits — see
    /// [`outbound::AuditClaims`]. Locks for exactly the length of the claim, so
    /// nothing is held across the SSRF screen's DNS `await`.
    ///
    /// Without a registry entry (no tab identity, or `gate` has not run) there
    /// is no session to attribute a repeat to, so the claim falls back to
    /// `unscoped` — which since #48 F-40 is the identity-less scope's own
    /// process-global ledger ([`outbound::UnscopedAudit`]) and **not** a
    /// constant. The latch and the budget still fail open here; the ROWS no
    /// longer do, because "no session" was never a reason for a caller to be
    /// able to write one row per event into a capped lane.
    ///
    /// `unscoped` is a closure so the fallback ledger is touched only when it is
    /// actually reached, and — load-bearing — so its lock is never taken while
    /// this one is held. The early `return` inside the `if let` is what ends the
    /// registry borrow before that call.
    pub(super) fn claim<T>(
        &self,
        scope: Option<&LatchScope>,
        claim: impl FnOnce(&mut outbound::Budget) -> T,
        unscoped: impl FnOnce() -> T,
    ) -> T {
        let Some(scope) = scope else { return unscoped() };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tabs.get_mut(&scope.key()) {
            return claim(&mut entry.budget);
        }
        drop(tabs);
        unscoped()
    }

    /// The `/status` view: one row per tab the proxy has served, sorted so the
    /// output is stable to eyeball across polls.
    pub(super) fn snapshot(&self) -> Vec<LatchStatus> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<LatchStatus> = tabs
            .iter()
            .map(|((agent, tab), st)| LatchStatus {
                consumer: agent,
                tab: tab.clone(),
                session: st.session.clone(),
                view: st.view(),
            })
            .collect();
        rows.sort_by(|a, b| (a.consumer, &a.tab).cmp(&(b.consumer, &b.tab)));
        rows
    }
}

/// One `/status` latch row.
#[derive(Serialize, Debug)]
pub struct LatchStatus {
    pub consumer: &'static str,
    pub tab: String,
    pub session: Option<String>,
    /// V32 Phase F: the latch label plus the contamination bit and per-row
    /// override availability. **Flattened**, so the wire shape is unchanged for
    /// the Phase B readers (`latch` stays a top-level key of the row) and the
    /// new facts sit beside it rather than in a nested object — one row per
    /// tab, as `/status` has always been.
    #[serde(flatten)]
    pub view: LatchView,
}

impl LatchStatus {
    /// [`Latch::label`] for this row: `open` / `external` / `local`.
    ///
    /// Read by the tests (and by anyone holding a snapshot); the wire form goes
    /// through `view`'s flattened `latch` key, so the running app never calls
    /// this — the same `cfg_attr` shape `toolclass::mutates_fs` carried until
    /// V33 Phase F landed its consumer and removed it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latch(&self) -> &'static str {
        self.view.latch
    }
}

/// The process-wide registry. Latch state is intentionally in-memory and
/// non-durable: it describes *live* conversations, and an app restart
/// necessarily ends every one of them.
///
/// **It is bounded** — one entry per (agent, tab) pair, over the AI tab ids the
/// user has configured, which are reused across restarts, so nothing
/// accumulates over a long-running app. That was asserted here and enforced
/// nowhere until #45: every key arrives in a request body, and the map has no
/// TTL, no cap and no eviction, while every entry is serialized into every
/// `/status` response and every 4 s `latch_status` poll. The bound is now real
/// and tested — [`is_configured_tab`], applied in [`latch_scope`], which is the
/// one funnel through which `gate` and `beacon` (the only two methods that
/// insert) receive a scope at all.
///
/// The caveat that keeps this honest: the bound is `configured AI tabs × 1`
/// only while the settings snapshot is readable. See [`is_configured_tab`]'s
/// empty-list escape.
pub(super) fn latches() -> &'static LatchRegistry {
    static LATCHES: OnceLock<LatchRegistry> = OnceLock::new();
    LATCHES.get_or_init(LatchRegistry::default)
}

/// One tab session's audit-row claim ledger, as the SSRF chokepoint and the
/// detection boundary see it (#48).
///
/// The ledger itself lives inside the tab's [`outbound::Budget`], which is the
/// only per-conversation state with the right lifetime *and* the right reset
/// rule: `TabLatch::observe` wipes it on a proved session rotation, so a
/// genuinely new conversation is entitled to its own rows. A process-global
/// `HashSet<scope>` was the alternative and is wrong for exactly that reason —
/// proxy scopes are stable `agent:tab` strings, so it would suppress a tab's
/// rows permanently, across every session it ever holds.
///
/// A handle rather than a borrow because the ledger sits behind the registry
/// mutex, which must not be held across the SSRF screen's DNS `await`.
///
/// The second field is the agent whose identity-less ledger a call **without** a
/// scope claims against ([`outbound::UnscopedAudit`], #48 F-40). It is carried
/// rather than derived because at the one construction site the agent is already
/// resolved through `graph::source_for_consumer` — the same normalisation that
/// builds the `agent:(no tab identity)` label the resulting row shows, so the
/// ledger and the label cannot name different things.
pub(super) struct TabAudit<'a>(pub(super) Option<&'a LatchScope>, pub(super) &'static str);

impl TabAudit<'_> {
    /// This call's fallback ledger. One place, so both claims agree on it.
    fn unscoped(&self) -> outbound::UnscopedAudit {
        outbound::UnscopedAudit::for_agent(self.0.map(|s| s.agent).unwrap_or(self.1))
    }
}

impl outbound::ScopeAudit for TabAudit<'_> {
    fn claim_ssrf(&self) -> outbound::DoublingRow {
        latches().claim(self.0, outbound::Budget::claim_ssrf_flag, || {
            self.unscoped().claim_ssrf()
        })
    }
    fn claim_unscreened(&self) -> bool {
        latches().claim(self.0, outbound::Budget::claim_unscreened_flag, || {
            self.unscoped().claim_unscreened()
        })
    }
}

/// V32 Phase F: the `/status` latch rows, read **in process**.
///
/// The per-tab taint badge and its override popover live in the webview, which
/// has no bearer token and no business acquiring one — every loopback route is
/// authenticated precisely so only cImp-spawned children can reach it. The
/// Tauri backend already owns the registry, so the UI goes through an IPC
/// command ([`crate::ipc::commands::latch_status`]) that calls this, and the
/// token never leaves the processes that need it.
///
/// **Step 4: it folds each tab's current live session in first.** See
/// [`LatchRegistry::observe_all`] for why the read path has to — a Claude tab
/// polls no `/latch/state`, so without this an armed one-shot would wait for the
/// model to call a cImp tool rather than for the user's `/clear`. This is the
/// same 4 s read the badge already makes; no second timer is introduced.
pub fn latch_snapshot(ctx: &RouteCtx) -> Vec<LatchStatus> {
    // Resolve scopes with the registry lock NOT held: `latch_scope` locks the
    // graph service for the live-session lookup.
    let settings = ctx.settings();
    let scopes: Vec<LatchScope> = latches()
        .keys()
        .iter()
        .filter_map(|(agent, tab)| {
            latch_scope(ctx, &settings, agent, Some(tab.as_str())).into_scope()
        })
        .collect();
    for cleared in latches().observe_all(&scopes) {
        cleared.record();
    }
    latches().snapshot()
}

/// The caller-composed parts of one Phase F `injection_flag` row: its
/// provenance, its `tool` column and the prose an incident reviewer reads.
///
/// **They are built together on purpose (#48).** #45 shipped them apart — the
/// detail functions spelled `Origin::Ipc` / `Origin::Http` into their own
/// format strings while `Flag.origin` was set independently at the call site.
/// #47 then made `origin` a required field precisely so provenance could not be
/// taken by omission, but the sentence a human actually reads was still not
/// derived from it: re-expose an HTTP path into the override and the row's
/// `origin` key would say `http` while its text went on asserting that a human
/// clicked, with nothing to catch it. One struct, one origin, both consumers.
pub(super) struct FlagRow {
    /// Which feed lane the row belongs in. Carried here since step 4, because
    /// one of the four override actions is not a latch move at all — it releases
    /// the contamination bit, and that belongs in
    /// [`outbound::Screen::ContaminationCleared`] beside the row that SET the
    /// bit rather than among the latch moves. Deciding it here keeps the choice
    /// beside the sentence that describes it.
    pub(super) screen: outbound::Screen,
    /// Copied verbatim into [`outbound::Flag::origin`], and interpolated into
    /// [`detail`](Self::detail) by the same function that received it.
    pub(super) origin: outbound::Origin,
    /// The row's at-a-glance `tool` column.
    pub(super) tool: String,
    /// The row's human-readable body.
    pub(super) detail: String,
}

/// An override's `injection_flag` row (#45), composed from the origin the
/// caller states rather than one baked in here (#48).
///
/// Split out of [`apply_latch_override`] so the row's content is assertable
/// without an `AppHandle`, which this crate has no mock for — every Phase F
/// test called [`LatchRegistry::apply_override`] directly and stopped short of
/// the row, leaving the one artifact an incident review actually reads
/// uncovered.
pub(super) fn override_row(
    origin: outbound::Origin,
    action: LatchOverride,
    outcome: &OverrideOutcome,
) -> FlagRow {
    // The action is the row's at-a-glance "tool" for the three latch-shaped
    // moves: these rows have no tool call behind them, and what the user DID is
    // the fact worth reading. The clear names its own basis instead.
    let (screen, tool) = match action {
        LatchOverride::ClearContamination => (
            outbound::Screen::ContaminationCleared,
            ClearBasis::Resume.tool().to_string(),
        ),
        _ => (outbound::Screen::LatchOverride, action.as_str().to_string()),
    };
    let detail = match action {
        // Step 4: composed by the SAME function the armed-rotation clear uses,
        // so the two paths cannot describe one state change two ways.
        LatchOverride::ClearContamination => clear_detail(
            ClearBasis::Resume,
            origin,
            outcome
                .prior_taint
                .as_ref()
                .map_or(outcome.prior.label(), |p| p.latch),
            outcome
                .prior_taint
                .as_ref()
                .and_then(|p| p.session.as_deref()),
            None,
        ),
        // Step 4: the arm. It clears nothing, and the row has to say so — a
        // reader who sees "restore" in the feed and no `contamination_cleared`
        // row afterwards must be able to tell "still waiting" from "lost".
        LatchOverride::AwaitSessionClear => format!(
            "USER OVERRIDE (await_session_clear, origin: {}): a checkpoint was restored for this \
             tab, and the contamination flag is deliberately NOT cleared (contaminated={}). \
             Restoring rolls back FILES; it cannot remove injected text from the model's context \
             window, so this is the case where clearing immediately would be least justified. cImp \
             will clear the flag when it observes this tab start a new harness session — run \
             `/clear` in the tab, or restart it. Until then memory writes stay quarantined and \
             external results keep their envelope. Latch unchanged ({}).",
            origin.as_str(),
            outcome.view.contaminated,
            outcome.view.latch,
        ),
        // The flip is a WORKFLOW step, not a verdict, which is the whole reason
        // decision 15's 2026-08-10 amendment narrowed "contamination outlives
        // the override" to this one action. The row is where a reviewer learns
        // which of the two moves they are looking at.
        LatchOverride::FlipLocal => format!(
            "USER OVERRIDE (flip_local, origin: {}): taint latch {} → {}. Contamination is NOT \
             cleared by the flip (contaminated={}): memory writes stay quarantined and external \
             results keep their envelope, because the injected content is still in the \
             conversation and \"switch to local\" says \"research done, now apply it\" — not \"that \
             content was harmless\". Clearing the flag is its own decision with its own three \
             actions: `clear_contamination` (the user judges the content harmless), `unlatch` (the \
             user restores FULL access and accepts the larger risk) and `await_session_clear` \
             (after a restore, effective once a new harness session is observed). No automatic \
             path and no HTTP route can reach any of them.",
            origin.as_str(),
            outcome.prior.label(),
            outcome.view.latch,
            outcome.view.contaminated,
        ),
        // Decision 15's 2026-08-10 amendment. One click, two effects, and the
        // row states both — including whether the second one actually fired: an
        // unlatch on an uncontaminated tab clears nothing, and this sentence
        // must not be readable as evidence that a bit was released.
        LatchOverride::Unlatch => format!(
            "USER OVERRIDE (unlatch, origin: {}): taint latch {} → {} — FULL access restored, \
             which recreates the read+web trifecta with any injected content still in the \
             conversation. {} Memory notes ALREADY quarantined STAY quarantined — promoting or \
             discarding them is the Memory view's own review (locked decision 10), a separate \
             consent surface.",
            origin.as_str(),
            outcome.prior.label(),
            outcome.view.latch,
            match outcome.prior_taint.as_ref() {
                Some(p) => format!(
                    "The contamination flag was cleared by the same click (prior state: \
                     contaminated=true, latch={}, session={}), and it is filed as its own \
                     `contamination_cleared` row beside the row that SET the bit. The trust root \
                     is AUTHORITY, not evidence: an attacker cannot click this, and the click \
                     already handed back the strictly more dangerous capability — so leaving \
                     persistent memory writes quarantined would have overruled a judgement the \
                     product had just asked the user to make. This tab's future `context_note` \
                     writes are stored clean again, and a fresh contamination will report itself \
                     as a new transition.",
                    p.latch,
                    p.session.as_deref().unwrap_or("unknown"),
                ),
                None => format!(
                    "This tab was not flagged as contaminated, so there was nothing to clear \
                     (contaminated={}).",
                    outcome.view.contaminated
                ),
            },
        ),
    };
    FlagRow {
        screen,
        origin,
        tool,
        detail,
    }
}

/// Decision 15's 2026-08-10 amendment: the `contamination_cleared` row a **full
/// unlatch** owes, or `None` when this override released nothing.
///
/// # Why a second row rather than a sentence in the first one
///
/// [`outbound::Screen::ContaminationCleared`] is a retention lane *and* a join
/// key: its own doc says a reviewer filtering the two contamination wire values
/// "gets one tab's whole taint lifecycle", and [`outbound::contamination_events`]
/// queries exactly those two lanes. A release visible only inside a
/// [`outbound::Screen::LatchOverride`] detail string is invisible to that join —
/// the Workbench Timeline would show a `☣` that never closes, for a tab the
/// registry reports clean. That is the "signal with no consumer" class (#48,
/// F-3) reintroduced one amendment later, so the clear is filed where every
/// other clear is filed.
///
/// # Why it is composed here and not in [`LatchRegistry::apply_override`]
///
/// The origin is stated ONCE, by the caller (#48, A2-3): [`apply_latch_override`]
/// is the only path in and it names [`outbound::Origin::Ipc`] for both rows, so
/// the two halves of an override cannot disagree about who acted. Composing it
/// here also makes it assertable without an `AppHandle`, which this crate has no
/// mock for — the same seam [`override_row`] exists for.
///
/// `None` covers the honest case: an unlatch on a tab that was never
/// contaminated. It is not an error, and it must not write a row saying a bit
/// was released.
pub(super) fn unlatch_clear_row(
    origin: outbound::Origin,
    action: LatchOverride,
    scope: &LatchScope,
    outcome: &OverrideOutcome,
) -> Option<ContaminationCleared> {
    if action != LatchOverride::Unlatch {
        return None;
    }
    let prior = outcome.prior_taint.as_ref()?;
    Some(ContaminationCleared {
        origin,
        basis: ClearBasis::Unlatch,
        consumer: scope.agent,
        scope: scope.label(),
        // The conversation the bit was cleared FOR, exactly as the resume path
        // files it: the one the `contamination` row named.
        session: prior.session.clone(),
        root: scope.root.clone(),
        detail: clear_detail(
            ClearBasis::Unlatch,
            origin,
            prior.latch,
            prior.session.as_deref(),
            None,
        ),
    })
}

/// V32 Phase F (locked decision 15): apply a user-initiated latch move to one
/// tab, write its `injection_flag` row, and return the tab's new view.
///
/// **Reachable from the `latch_override` Tauri IPC command only** — the badge
/// popover, i.e. the user. `POST /latch/override` existed alongside it until
/// #45 "so the same action is reachable from a child or a live-verification
/// script"; that convenience made a capability GRANT drivable by anything
/// holding the launch token, and left the resulting row indistinguishable from
/// a click. There is no HTTP path into this function now, so
/// [`outbound::Origin::Ipc`] on the row is a fact rather than an assumption.
pub fn apply_latch_override(
    ctx: &RouteCtx,
    consumer: &str,
    tab: &str,
    action: &str,
) -> Result<LatchView, String> {
    let action = LatchOverride::parse(action)?;
    let agent = crate::graph::source_for_consumer(consumer);
    // One settings snapshot, shared with the tab-id check inside `latch_scope`.
    let settings = ctx.settings();
    let scope = latch_scope(ctx, &settings, agent, Some(tab))
        .into_scope()
        .ok_or_else(|| {
            // #45 folded "not a configured tab" into this refusal, so the
            // message has to cover both — a popover that said "needs a tab id"
            // about a tab id it was given would send the user looking in the
            // wrong place.
            format!("a latch override needs a configured tab id (got {tab:?})")
        })?;
    let outcome = latches().apply_override(&scope, action)?;

    // Locked decision 15: "every override writes an `injection_flag` row … so
    // the feed records who opened what." `ok: true` — nothing was denied; this
    // is a capability GRANT, and the feed must show it as the deliberate act it
    // is rather than as a failure. The prior latch is in the detail because
    // "restored full access" from `external` and from `local` are very
    // different events.
    //
    // The origin is stated ONCE, here (#48): `override_row` puts it in the
    // row's `origin` key and in the sentence the reviewer reads, so the two
    // cannot come apart. `Ipc` is the one origin that means a human acted, and
    // it is a fact rather than an assumption only because no HTTP path into
    // this function survives (#45) — re-expose one and this constant is what
    // has to change.
    let row = override_row(outbound::Origin::Ipc, action, &outcome);
    outbound::record_flag(outbound::Flag {
        // Step 4: the row's own screen, not a constant here — a contamination
        // clear is filed beside the row that set the bit, not among the latch
        // moves. See `FlagRow::screen`.
        screen: row.screen,
        origin: row.origin,
        consumer: agent,
        scope: &scope.label(),
        // The scope is in hand — see `LatchScope::attribution`.
        attribution: scope.attribution(),
        session: scope.session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root: scope.root.clone(),
        detail: &row.detail,
    });
    // Decision 15's 2026-08-10 amendment: a full unlatch also RELEASES the
    // contamination bit, and that release owes the `contamination_cleared` lane
    // its own row — see `unlatch_clear_row` for why it is not folded into the
    // latch move's prose. Written AFTER the override row, in the order the state
    // moved: the latch reopened, and the flag went with it. Same stated origin
    // for both, from the one constant above.
    if let Some(cleared) = unlatch_clear_row(outbound::Origin::Ipc, action, &scope, &outcome) {
        cleared.record();
    }
    Ok(outcome.view)
}

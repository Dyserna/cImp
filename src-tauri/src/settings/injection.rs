//! V32 Phase G (locked decision 16) — the **three-level enable hierarchy** and
//! the ONE function every V32 enforcement site resolves through.
//!
//! # Why this exists
//!
//! Roughly half of V32 shipped structurally always-on: the taint latch, the
//! spotlighting envelope, the SSRF guard, the canary, memory quarantine,
//! consumer hygiene. A security control with no escape hatch becomes a reason to
//! stop using the application the first time it misfires on real work — so the
//! milestone's answer is not "fewer controls" but "three levels of switch, and
//! one place that resolves them".
//!
//! - **L1 — the global master.** [`InjectionSettings::protection`] (default
//!   `true`). Off disables every V32 **containment** control everywhere: all
//!   tabs AND the offload worker. It is the one switch nothing overrides
//!   upward. Its reach is [`Feature::master_gated`], and exactly one feature is
//!   outside it — [`Feature::ToolSteering`], the V38 managed-tool nudge, which
//!   is a token-efficiency preference rather than a security posture and has
//!   its own L2/L3 switches instead (see that predicate for the why).
//! - **L2 — per feature, app-wide.** One `<feature>_enabled` flag per
//!   [`Feature`], **all defaulting `true`** since the V39 posture decision
//!   ("master on, every sub-protection on"). One deliberate shape exception
//!   remains: [`Feature::NativeWeb`], whose L2 is a tri-mode rather than a
//!   boolean (see *Native-web reconciliation* below).
//! - **L3 — per scope.** A tri-state [`Override`] (`Inherit` | `On` | `Off`,
//!   default `Inherit`) stored per scope, per feature.
//!
//! # Where the posture actually lives now (V39)
//!
//! L1 and L2 ship fully on, and **a newly created AI tab's L3 row ships all
//! `Off`** ([`TabInjectionOverrides::all_off`]). The per-tab row is therefore
//! the switch the user actually reaches for, from the tab's shield badge, and
//! the app-wide levels are the ceiling above it rather than the thing that has
//! to be edited per install.
//!
//! Two consequences are load-bearing and stated where they are enforced:
//!
//! - **`Override::default()` is still `Inherit`.** The all-off row is a
//!   *tab-creation* default, not a serde default: a cell absent from a settings
//!   file must keep resolving to L2, or an upgrade would silently change the
//!   posture of every existing tab. Schema step 34 → 35 writes an explicit
//!   `inherit` into every absent cell of every existing AI tab for exactly that
//!   reason, so the new all-off default applies only to tabs created afterwards.
//! - **A tab cell that is `Off` is the tab BASELINE, not reduced protection.**
//!   [`protection_reduced`] therefore ignores a tab row that is off *because of
//!   the tab's own cell* — see that function for the exact predicate and for why
//!   it does not ignore the whole tab scope.
//!
//! # The locked resolution rule
//!
//! ```text
//! if master_gated && !L1 { false }
//! else { match L3 { On => true, Off => false, Inherit => L2 } }
//! ```
//!
//! An L3 `On` CAN re-enable a feature its L2 default disabled — that is what an
//! override means. NOTHING re-enables past an L1 `off` — that is what a master
//! switch means, for everything the master switch is *about*
//! ([`Feature::master_gated`]). [`decide`] is the single implementation;
//! [`effective`] is the boolean-only shorthand.
//!
//! # The cross-module invariant
//!
//! **No enforcement site reads a raw settings field.** Every one calls
//! [`effective`] (or one of the small resolved-value helpers below —
//! [`budget_limits`], [`detection_config`], [`native_web_mode`] — which call it
//! themselves). Otherwise a future control would silently ignore a level, and
//! the master switch would be a master switch of everything except the newest
//! thing.
//!
//! The invariant is **structural** (#44). Every L1/L2 switch on
//! [`InjectionSettings`](super::schema::InjectionSettings), every L3 cell on
//! [`TabInjectionOverrides`] / [`WorkerInjectionOverrides`], and the two fields
//! that hold those rows (`InjectionSettings::worker`,
//! `AiToolTabConfig::injection_overrides`) are `pub(in crate::settings)`: naming
//! one from an enforcement site is a **privacy error**, not something a reviewer
//! or a source scan has to catch. It was watched by a scanning tripwire
//! (`src/injection_tripwire.rs`) until #44 retired it — a scan is a strictly
//! weaker restatement of "only module X may name field Y", and it had three
//! bypasses, two of which needed no intent at all.
//!
//! The one thing privacy cannot express is the level *above* the fields: that
//! every [`Feature`] has L2 storage of its own. That is
//! `every_feature_has_a_guarded_l2_field` in this module's tests, relocated here
//! when the tripwire went.
//!
//! # Scope is not always a tab
//!
//! Tabs are the scope for the consumer-side controls, but the offload worker is
//! a task-scoped service with no tab. It is a first-class pseudo-scope
//! ([`Scope::OffloadWorker`]) with its own L3 row, and the features that exist
//! only there ([`Feature::Canary`]) carry only that row.
//!
//! The remaining two are the halves of what used to be one `Scope::App`
//! (#48, F-35, locked decision 36). [`Scope::AppWide`] is the app-wide baseline
//! and nothing else — L1 ∧ L2 — the scope of a control with no per-scope row at
//! all ([`Feature::TerminalEscapeHygiene`] — TTS and toasts are global surfaces
//! per the global-only avatar/TTS decision). [`Scope::UnknownCaller`] is a real
//! call from a real tab whose identity did not arrive: the app-wide baseline
//! **plus** any configured tab's L3 `On` (N-1). One variant answered both
//! questions under the first one's name, which produced two defects — M-21's
//! false claim and F-35's suppressed signal — before it was split.
//!
//! Which features carry which rows is expressed *structurally*, not by
//! convention: [`TabInjectionOverrides`] and [`WorkerInjectionOverrides`] are
//! separate structs carrying only their own scope's features, so "the canary has
//! no per-tab row" is a fact about the types rather than a rule someone has to
//! remember. [`Feature::has_tab_scope`] / [`Feature::has_worker_scope`] mirror
//! that for the UI and the introspection API, and a test pins the two views
//! against each other.
//!
//! # Native-web reconciliation (decision 14 meets decision 16)
//!
//! [`Feature::NativeWeb`] already had an `off` value before this phase:
//! `native_web_visibility` is a tri-mode (`off` | `sensor` | `deny`) and its
//! `off` IS this feature's disabled state. It therefore gets **no L2 boolean of
//! its own** — the mode *is* its L2. Storing both would make a contradictory
//! state representable (`enabled = false` beside `mode = "deny"`), and the one
//! thing a three-level hierarchy cannot afford is a fourth, informal level.
//!
//! The consequence, spelled out because it is the only non-obvious corner of the
//! resolution table: an L3 `On` over an app-wide `off` re-enables the feature at
//! its **default posture, `sensor`** ([`native_web_mode`]). "On" has to mean
//! something, and the mode's own default is the only honest reading — `deny`
//! would take a tool away from one tab because the user disabled the feature
//! everywhere else.
//!
//! # Spawn-baked vs live
//!
//! [`Feature::spawn_baked`] names the features whose value is baked into a tab
//! when it launches: [`Feature::NativeWeb`], [`Feature::ConsumerHygiene`],
//! Phase H's [`Feature::OpencodeNativeGate`] (whose flag is compiled into the
//! generated OpenCode plugin), and — since #48's M-3 — [`Feature::Spotlighting`],
//! which `tabs::config::fact_promotion_block` reads at launch to decide whether
//! the pinned-memory addendum enters the system prompt ENVELOPED. Their L2 *and*
//! L3 values ride `tabs::config::spawn_inject_sig` via [`spawn_sig`], so
//! flipping any of them raises the restart hint.
//!
//! Spotlighting is the one control in **both** columns — the addendum is baked,
//! the proxy's EXTERNAL envelope is per call — and the predicate resolves that
//! by asking "does the user owe this a restart?", not "is it ever live?". A
//! spawn-time call site can no longer disagree with the list:
//! [`Feature::baked_at_spawn`] is const-asserted at the site.
//!
//! Every other feature resolves per call and takes effect immediately.

use serde::{Deserialize, Serialize};

use super::schema::{Settings, TabConfig};

// ── Features ───────────────────────────────────────────────────────────────

/// Declare [`Feature`] and [`Feature::ALL`] from one list, so the array cannot
/// drift from the enum (#47).
///
/// It used to be hand-written beside the enum, and a variant left out of it was
/// invisible to `report`, [`protection_reduced`], [`spawn_sig`], the Settings
/// matrix **and** the test that was supposed to guard it — which iterated the
/// array rather than the enum, so an omission removed the feature from its own
/// coverage. The exhaustive matches below would still have caught the addition,
/// but "the compiler makes you name it somewhere" is not the same property as
/// "every surface that enumerates features sees it".
///
/// The invocation reads exactly like the enum declaration it replaces —
/// attributes, doc comments and all — because the alternative (a bespoke
/// list-of-variants syntax) trades one drift risk for a worse readability one.
macro_rules! declare_features {
    (
        $(#[$enum_attr:meta])*
        pub enum $name:ident {
            $( $(#[$variant_attr:meta])* $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$enum_attr])*
        pub enum $name {
            $( $(#[$variant_attr])* $variant, )+
        }

        impl $name {
            /// Every feature, in declaration order — which is the order the
            /// Settings UI and the introspection report render them
            /// (cheapest/most-structural first, spawn-baked last).
            ///
            /// Derived from the variant list above, not written out: see
            /// [`declare_features`].
            pub const ALL: &'static [$name] = &[ $( $name::$variant, )+ ];
        }
    };
}

declare_features! {
/// One V32 control, as the hierarchy addresses it.
///
/// The enum — rather than a string key — is what makes "every feature is
/// resolved, and resolved the same way" checkable: [`Feature::ALL`] drives the
/// truth-table tests, the introspection report and the Settings UI, so a control
/// added without a row here has nowhere to appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// The bidirectional taint latch: the worker's `latch_gate` + def filtering
    /// and the loopback proxy's `gate`.
    TaintLatch,
    /// The nonced data-not-instructions envelope on EXTERNAL results and on
    /// recalled memory.
    Spotlighting,
    /// The detection surface (signature + classifier), parent of the two
    /// existing per-layer sub-toggles.
    Detection,
    /// The outbound URL range screen at `McpHost::call_recorded`.
    SsrfGuard,
    /// Per-scope EXTERNAL call/byte caps. The existing numerics stay the tuning
    /// knobs; this is the on/off above them.
    FetchBudgets,
    /// The in-band canary. Worker-only: consumers' system prompts are not
    /// cImp-authored, so there is nothing of ours in them to leak.
    Canary,
    /// Quarantine of `context_note` writes made by a contaminated conversation.
    MemoryQuarantine,
    /// Native-web visibility (locked decision 14). Its L2 is the tri-mode
    /// itself — see the module docs.
    NativeWeb,
    /// The pinned OpenCode permission block + the injection-hygiene guidance
    /// addendum. Both spawn-baked.
    ConsumerHygiene,
    /// V38 follow-up — the **managed-tool steering paragraph**: one fixed,
    /// generic nudge in the same guidance channel as
    /// [`Feature::ConsumerHygiene`]'s, asking the harness to prefer cImp's
    /// `run_check` / `run_command` MCP tools over running the same commands in
    /// its own built-in shell.
    ///
    /// Spawn-baked (it is written into `--append-system-prompt` / the managed
    /// instructions file at launch) and read by both consumers. Deliberately
    /// **not** an enumeration of check or binary names: the tools' own enums are
    /// self-describing and update live, an injected prompt cannot, and a
    /// paragraph that listed them would move the spawn signature on every
    /// registry edit.
    ToolSteering,
    /// V32 Phase H (locked decision 17): the OpenCode plugin's
    /// `tool.execute.before` handler *denying* the harness's own native tools
    /// against the tab's taint latch, rather than only beaconing on them.
    ///
    /// **Its L2 default was `false` until V39** — decision 17's reasoning was
    /// that whole-surface denial of `bash`/`read`/`edit` materially changes
    /// everyday tab UX and is an opt-in posture. That reasoning is kept as
    /// history on [`Feature::default_enabled`]; the V39 posture decision
    /// ("master on and every sub-protection on") supersedes the *default*, and
    /// the opt-in now lives one level down — a new tab's L3 row ships all `Off`
    /// ([`TabInjectionOverrides::all_off`]), so nothing is denied until the user
    /// enables it from the tab's shield badge.
    ///
    /// Spawn-baked (the flag is compiled into the generated plugin) and
    /// tab-scoped only: it is delivered by an OpenCode plugin, so the offload
    /// worker has no row for it and neither does any non-OpenCode consumer.
    OpencodeNativeGate,
    /// Stripping terminal control sequences out of external text cImp composes
    /// into non-HTML sinks. App-wide: TTS and toasts are global surfaces.
    TerminalEscapeHygiene,
}
}

impl Feature {
    /// The stable wire/UI key. Fixed strings, pinned by a test: they key the
    /// Settings matrix and the `/status` rows, so a rename is a wire change.
    pub fn key(self) -> &'static str {
        match self {
            Feature::TaintLatch => "taint_latch",
            Feature::Spotlighting => "spotlighting",
            Feature::Detection => "detection",
            Feature::SsrfGuard => "ssrf_guard",
            Feature::FetchBudgets => "fetch_budgets",
            Feature::Canary => "canary",
            Feature::MemoryQuarantine => "memory_quarantine",
            Feature::NativeWeb => "native_web",
            Feature::ConsumerHygiene => "consumer_hygiene",
            Feature::ToolSteering => "tool_steering",
            Feature::OpencodeNativeGate => "opencode_native_gate",
            Feature::TerminalEscapeHygiene => "terminal_escape_hygiene",
        }
    }

    /// Human label for the Settings matrix and the badge popover.
    pub fn label(self) -> &'static str {
        match self {
            Feature::TaintLatch => "Taint latch",
            Feature::Spotlighting => "Spotlighting envelope",
            Feature::Detection => "Injection detection",
            Feature::SsrfGuard => "SSRF guard",
            Feature::FetchBudgets => "Fetch budgets",
            Feature::Canary => "Canary (offload worker)",
            Feature::MemoryQuarantine => "Memory quarantine",
            Feature::NativeWeb => "Native-web visibility",
            Feature::ConsumerHygiene => "Consumer hygiene",
            Feature::ToolSteering => "Managed-tool steering",
            Feature::OpencodeNativeGate => "OpenCode native-tool gating",
            Feature::TerminalEscapeHygiene => "Terminal escape hygiene",
        }
    }

    /// This feature's **L2 default** — the value an untouched settings file
    /// resolves it to.
    ///
    /// **Every control ships on** since the V39 posture decision: the master is
    /// on and so is every sub-protection, and the per-install tailoring happens
    /// one level down, where a newly created AI tab's L3 row ships all `Off`
    /// ([`TabInjectionOverrides::all_off`]).
    ///
    /// # Why the predicate still exists (the Phase H history)
    ///
    /// Every V32 control before Phase H defaulted `true`, and one predicate read
    /// that as a law: "any feature resolving off means protection is REDUCED"
    /// ([`protection_reduced`], and its frontend twin `reducedFeaturesFor`).
    /// [`Feature::OpencodeNativeGate`] broke it — locked decision 17 shipped it
    /// **default off**, because whole-surface denial of `bash`/`read`/`edit`
    /// materially changes everyday tab UX and is an opt-in posture, not a
    /// baseline. Without this predicate a fresh install would have raised the
    /// reduced-protection chip on every tab out of the box, which is how an
    /// indicator stops being read.
    ///
    /// V39 keeps that *rationale* and moves the *mechanism*: the gate is opt-in
    /// per tab now, not app-wide, so its L2 joins the others at `true` while the
    /// rule this predicate encodes is unchanged. "Reduced" is still measured
    /// against the DEFAULT, not against `true` — a default-off control that is
    /// off is a baseline and one switched on is *more* protection — and the
    /// moment a future control ships off, that reading is already in place
    /// rather than something to rediscover.
    ///
    /// An exhaustive `match` rather than a `matches!` (#47): a new control's
    /// shipping default is a decision, and falling through to `true` would let
    /// it be taken by omission.
    pub fn default_enabled(self) -> bool {
        match self {
            Feature::TaintLatch
            | Feature::Spotlighting
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary
            | Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
            | Feature::ToolSteering
            | Feature::OpencodeNativeGate
            | Feature::TerminalEscapeHygiene => true,
        }
    }

    /// Whether the **L1 master switch** ([`InjectionSettings::protection`]) can
    /// switch this feature off.
    ///
    /// `true` for every V32 containment control — that is what a master switch
    /// means, and [`decide`] short-circuits on it.
    ///
    /// `false` for exactly one feature today: [`Feature::ToolSteering`]. The
    /// master switch is the documented escape hatch for *"a containment control
    /// is misfiring on my real work"* — flipping it says **"reduce my security
    /// posture"**. Managed-tool steering is not posture: it is a
    /// TOKEN-EFFICIENCY nudge, one paragraph asking the harness to prefer
    /// `run_check` / `run_command` over its own shell. It denies nothing, it
    /// screens nothing, and it cannot misfire in the way the escape hatch
    /// exists for. Riding L1 made a project that had turned protection off in
    /// its `.cimp/config.json` silently lose the nudge — a token regression
    /// caused by a security switch, which is a switch doing something its label
    /// does not say.
    ///
    /// The consequence for the rest of the hierarchy is stated where it
    /// matters: a non-master-gated feature resolves through the unchanged
    /// L3 → L2 path, and [`protection_reduced`] (with its frontend twin
    /// `reducedFeaturesFor`, via the report's `master_gated` field) never counts
    /// one — "reduced protection" is a claim about protection, and this is not
    /// one of those controls.
    ///
    /// An exhaustive `match` rather than a `matches!` (#47): whether a new
    /// control answers to the master switch is a decision, and defaulting to
    /// either answer would let it be taken by omission.
    pub fn master_gated(self) -> bool {
        match self {
            Feature::ToolSteering => false,
            Feature::TaintLatch
            | Feature::Spotlighting
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary
            | Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
            | Feature::OpencodeNativeGate
            | Feature::TerminalEscapeHygiene => true,
        }
    }

    /// Whether this feature carries a per-TAB L3 row. Mirrors
    /// [`TabInjectionOverrides`]'s field set (pinned by a test). Exhaustive so
    /// a new feature must state its scopes rather than inherit them (#47).
    pub fn has_tab_scope(self) -> bool {
        match self {
            Feature::TaintLatch
            | Feature::Spotlighting
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
            | Feature::ToolSteering
            | Feature::OpencodeNativeGate => true,
            Feature::Canary | Feature::TerminalEscapeHygiene => false,
        }
    }

    /// Whether this feature carries the `offload-worker` L3 row. Mirrors
    /// [`WorkerInjectionOverrides`]'s field set (pinned by a test).
    ///
    /// Memory quarantine is deliberately absent: the worker cannot dispatch
    /// `context_*` at all (issue #38) and serves a hard refusal instead, so a
    /// worker quarantine row would be a switch with no enforcement site behind
    /// it. Native-web visibility, consumer hygiene and the Phase H OpenCode
    /// native gate are absent because the worker is not a harness — it has no
    /// native tools and no spawn config.
    pub fn has_worker_scope(self) -> bool {
        match self {
            Feature::TaintLatch
            | Feature::Spotlighting
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary => true,
            Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
            | Feature::ToolSteering
            | Feature::OpencodeNativeGate
            | Feature::TerminalEscapeHygiene => false,
        }
    }

    /// Whether this feature is applied at TAB SPAWN — so a change to it does not
    /// reach a tab that is already running.
    ///
    /// The consumer of this predicate is `tabs::config::spawn_inject_sig` (via
    /// [`spawn_sig`]): a spawn-baked feature's L2/L3 values must move the
    /// signature so the user gets the restart hint, because a running tab keeps
    /// whatever posture it launched with. `SettingsApp.svelte` reads the same
    /// bit to render "(needs a tab restart)" beside the control.
    ///
    /// **Not the complement of "live" (#48, M-3).** It used to be worded "applied
    /// at TAB SPAWN *rather than* per call", and that framing is what let
    /// [`Feature::Spotlighting`] out: spotlighting is applied per call at the
    /// proxy AND baked at spawn, because `tabs::config::fact_promotion_block`
    /// decides at launch whether the pinned-memory addendum goes into the system
    /// prompt enveloped. Under the old wording nobody could say which list it
    /// belonged to, so it was in neither and the restart hint never fired —
    /// leaving a tab toggled ON mid-session injecting UNENVELOPED pre-V32 memory
    /// into its system prompt. The question this predicate answers is therefore
    /// "does the user owe this control a restart?", and *any* spawn-baked
    /// component is enough to answer yes.
    ///
    /// [`Feature::OpencodeNativeGate`] joins the pair in Phase H: its flag is
    /// baked into the generated OpenCode plugin, and the plugin is written at
    /// tab spawn.
    ///
    /// `const` so [`baked_at_spawn`](Self::baked_at_spawn) can turn a
    /// disagreement between this list and a spawn-time call site into a BUILD
    /// ERROR rather than a source-scan test that only sees the half it scans.
    ///
    /// The **frontend** mirror of this predicate's output
    /// (`SPAWN_BAKED_INJECTION_FEATURES` in `src/lib/settings/types.ts`) cannot be
    /// held by the compiler, so it is held by an `include_str!` tripwire instead:
    /// `settings::frontend_mirrors::spawn_baked_feature_mirror_is_current_in_both_directions`
    /// compares this filtered set against that array both ways. #48 F-27, second
    /// instance — that array is what raises the in-window restart hint, and it had
    /// already gone stale twice over.
    pub const fn spawn_baked(self) -> bool {
        match self {
            Feature::NativeWeb
            | Feature::ConsumerHygiene
            // The steering paragraph is written into the launch addendum beside
            // the hygiene one, so it owes the same restart hint.
            | Feature::ToolSteering
            | Feature::OpencodeNativeGate
            // #48 (M-3): baked by `fact_promotion_block` into the launch
            // addendum. Also live at the proxy — see the note above.
            | Feature::Spotlighting => true,
            Feature::TaintLatch
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary
            | Feature::MemoryQuarantine
            | Feature::TerminalEscapeHygiene => false,
        }
    }

    /// [`spawn_baked`](Self::spawn_baked) as a **compile-time assertion**, for
    /// the call sites that BAKE a control into a tab's launch.
    ///
    /// Used in a `const` item it is const-evaluated, so a feature resolved at
    /// spawn while `spawn_baked()` says otherwise fails the BUILD. That is the
    /// tripwire M-3 was missing and the V26 field report wanted: a source scan
    /// over `effective(` would have to be remembered by whoever adds the next
    /// spawn-time read, which is exactly the person who already forgot.
    ///
    /// Returns `self` so the call site reads as the feature it names:
    ///
    /// ```ignore
    /// const SPOTLIGHT_AT_SPAWN: Feature = Feature::Spotlighting.baked_at_spawn();
    /// ```
    pub const fn baked_at_spawn(self) -> Self {
        assert!(
            self.spawn_baked(),
            "this control is resolved at TAB SPAWN, so `Feature::spawn_baked` must return \
             true for it — otherwise `spawn_inject_sig` never moves and the user gets no \
             restart hint (#48, M-3)"
        );
        self
    }
}

// ── Scopes ─────────────────────────────────────────────────────────────────

/// Where a control is being resolved for.
///
/// Borrowed rather than owned so a gate on the hot path can build one without
/// allocating; `Copy` so it can be handed to several resolvers in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// One AI tab, keyed exactly as the latch registry keys it: the normalized
    /// `claude`/`opencode` agent vocabulary plus the cImp tab id.
    Tab { agent: &'a str, tab: &'a str },
    /// The offload worker — a task-scoped service with no tab.
    OffloadWorker,
    /// **The app-wide baseline and nothing else: L1 ∧ L2.**
    ///
    /// The honest answer for a control that has no per-scope row at all
    /// ([`Feature::TerminalEscapeHygiene`]) and for any surface that must report
    /// what the *application* is configured to do. It never borrows another
    /// scope's answer — that is what distinguishes it from
    /// [`Scope::UnknownCaller`], and the distinction is the whole of F-35
    /// (locked decision 36).
    AppWide,
    /// **A real call from a real tab whose identity did not arrive** (#48, N-1).
    ///
    /// Resolves to the app-wide baseline **plus every L3 `On` any configured AI
    /// tab states**: see [`decide`]. Over-protective on purpose, because the
    /// caller *is* one of those tabs and we cannot say which. The elevation is
    /// one-directional (an L3 `Off` never travels up), so it can only ever add
    /// protection.
    ///
    /// **Not the worker.** The offload worker always has an identity and always
    /// resolves through [`Scope::OffloadWorker`], so it is never the caller
    /// behind an identity-less call; folding its row in here would raise
    /// protection for a population this scope does not describe. The question
    /// *"is this control armed ANYWHERE, worker included?"* is a different one
    /// and has its own function, `armed_anywhere` — deliberately not a fourth
    /// variant, because every helper keyed on `Scope` would then accept it and
    /// silently deny one scope on another's behalf.
    ///
    /// Shares [`APP_SCOPE_KEY`] with [`Scope::AppWide`]: the split is a
    /// vocabulary change in Rust, never a wire change (see [`Scope::key`]).
    UnknownCaller,
}

impl<'a> Scope<'a> {
    /// The scope for a consumer-side call: the tab when the child sent an
    /// identity, [`Scope::UnknownCaller`] otherwise.
    ///
    /// `None`/empty is the **fail-open** case, and it resolves to the app-wide
    /// answer rather than to "off": V28's discipline is that a tool call must
    /// never fail for lack of identity, and a pre-`--tab` child losing its
    /// protection would be a silent downgrade rather than a graceful one.
    ///
    /// "Fail-open" is **relative to L2, not absolute** (#48, N-1). It reads as
    /// graceful only while L2 ≥ L3, and locked decision 17 ships the exact
    /// configuration that inverts it: "one hardened OpenCode tab, everything
    /// else as it was" is an L3 `On` over an L2 `Off`, and an app-wide answer of
    /// `off` would run a call from that hardened tab unprotected — while the
    /// Settings matrix still shows `→ on (this scope)` for it. Reachable today:
    /// a user-configured MCP entry invoking `cimp --offload-mcp` without
    /// `--tab`, or a generated `--mcp-config` written before V28. [`decide`]
    /// therefore honours any tab's L3 `On` at this scope — over-protective for a
    /// caller with no identity, which is the correct direction for a control
    /// whose failure mode is silent under-enforcement.
    ///
    /// **This mapping is the only thing that distinguishes the two app-level
    /// variants at the consumer boundary** (#48, F-35): a call that could not
    /// name its tab lands on [`Scope::UnknownCaller`], never on
    /// [`Scope::AppWide`], because the app-wide baseline is a statement about
    /// the *application* and this is a statement about a *caller*.
    pub fn for_tab(agent: &'a str, tab: Option<&'a str>) -> Self {
        match tab.map(str::trim).filter(|t| !t.is_empty()) {
            Some(tab) => Scope::Tab { agent, tab },
            None => Scope::UnknownCaller,
        }
    }

    /// A tab scope with no agent label.
    ///
    /// Override resolution keys on the **tab id alone** — ids are unique across
    /// agents, and one tab has exactly one harness — so `agent` exists for the
    /// scope key and the activity/report vocabulary, never for lookup. This
    /// constructor is for the callers that legitimately have no agent in hand
    /// (the Settings matrix, the spawn signature) and would otherwise invent
    /// one.
    pub fn tab_only(tab: &'a str) -> Self {
        Scope::Tab { agent: "", tab }
    }
}

impl Scope<'_> {
    /// The stable wire/UI key for this scope.
    ///
    /// **Both app-level variants key as [`APP_SCOPE_KEY`], deliberately**
    /// (#48, F-35): splitting them is a Rust vocabulary change, and the wire
    /// must not move with it. A key of its own for [`Scope::UnknownCaller`]
    /// would grow `/status` a scope the Settings matrix does not know and would
    /// render an unwritable row for it. The arm is load-bearing rather than
    /// defensive — `audit::mcp::Delivery` keys the activity row's `scope` column
    /// off a scope built by [`Scope::for_tab`], so a proxied audit with no
    /// `--tab` reaches it in production.
    pub fn key(&self) -> String {
        match self {
            Scope::Tab { agent, tab } => format!("{agent}:{tab}"),
            Scope::OffloadWorker => WORKER_SCOPE_KEY.to_string(),
            Scope::AppWide | Scope::UnknownCaller => APP_SCOPE_KEY.to_string(),
        }
    }
}

/// The `offload-worker` pseudo-scope's key, as it appears in `/status`, the
/// introspection command and the Settings matrix. A const because three
/// surfaces must agree on it.
pub const WORKER_SCOPE_KEY: &str = "offload-worker";

/// The app-wide scope's key.
pub const APP_SCOPE_KEY: &str = "app";

// ── The tri-state override ─────────────────────────────────────────────────

/// One L3 cell: inherit the app-wide L2 answer, or state one for this scope.
///
/// Serialized as a lowercase string and parsed **post-hoc**, following the C3
/// `updater::Mode` / Phase F `NativeWebVisibility` discipline: a settings file
/// is hand-editable, so an unrecognized value must resolve to something safe
/// rather than quarantine the whole file. Unknown reads as [`Override::Inherit`]
/// — the neutral cell. A typo must neither grant a scope protection the user did
/// not ask for nor take away protection they did not remove; deferring to the
/// app-wide answer is the only reading that does neither.
///
/// **The typo is not always a string** (#48, G-1). `#[serde(from = "String")]`
/// made the post-hoc parse cover unrecognized *strings* only, and
/// `#[serde(default)]` on the rows below fires for an ABSENT key, never for a
/// key whose value fails to type. So `"taint_latch": true` — the intuitive typo,
/// since the control it overrides *is* a boolean — and `"taint_latch": null` —
/// the intuitive way to clear a cell — failed the typed parse of the whole
/// settings file, which `settings::persistence` quarantines and replaces with
/// seeded defaults: themes, tabs, backends, checks, MCP servers and pricing all
/// reset because one cell was hand-edited wrong. The [`Deserialize`] impl below
/// therefore reads ANY JSON shape and keeps the same neutral answer; only the
/// two recognized words move a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(into = "String")]
pub enum Override {
    /// Take the app-wide L2 value.
    #[default]
    Inherit,
    /// On for this scope, even if L2 is off.
    On,
    /// Off for this scope, even if L2 is on.
    Off,
}

impl Override {
    /// Parse a stored cell. See the type docs for why unknown ⇒ `Inherit`.
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "on" => Override::On,
            "off" => Override::Off,
            _ => Override::Inherit,
        }
    }

    /// The canonical wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Override::Inherit => "inherit",
            Override::On => "on",
            Override::Off => "off",
        }
    }
}

/// Hand-written rather than `#[serde(from = "String")]` (#48, G-1): the derived
/// form only reaches [`Override::parse`] once the value has already typed as a
/// string, so every non-string shape failed the parse and quarantined the
/// settings file instead of resolving to the neutral cell.
///
/// Deserializing through [`serde_json::Value`] first is what makes the fallback
/// total — `true`, `null`, `1`, `[]`, `{}` and a nested object all land in the
/// catch-all arm. Settings are JSON on every path (`from_str` at load,
/// `from_value` after migration), so requiring a self-describing format costs
/// nothing here.
impl<'de> Deserialize<'de> for Override {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(s) => Override::parse(&s),
            // Every other shape is a hand-edit that did not mean anything the
            // hierarchy can honour. Neutral, and NOT an error.
            _ => Override::Inherit,
        })
    }
}

impl From<Override> for String {
    fn from(o: Override) -> Self {
        o.as_str().to_string()
    }
}

// ── L3 storage ─────────────────────────────────────────────────────────────

/// The per-TAB L3 row, stored on `AiToolTabConfig`.
///
/// Only the features that have a tab scope have a field, so a per-tab canary
/// override is not a thing a settings file can express — the illegal state is
/// unrepresentable rather than merely untested. Additive `#[serde(default)]`:
/// an untouched config deserializes to all-`Inherit`, i.e. exactly the
/// pre-Phase-G behaviour.
///
/// **`Default` is NOT the tab-creation default (V39).** A tab created today
/// gets [`TabInjectionOverrides::all_off`]; a cell *absent from a file* still
/// reads `Inherit`. The two must stay different, because they answer different
/// questions: "what does a new tab want?" and "what did a user who never wrote
/// this cell mean?". Collapsing them — by moving the serde default, or by
/// changing [`Override::default`] — would silently switch every existing tab's
/// posture on upgrade, which is precisely what the 34 → 35 schema step exists to
/// prevent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabInjectionOverrides {
    pub(in crate::settings) taint_latch: Override,
    pub(in crate::settings) spotlighting: Override,
    pub(in crate::settings) detection: Override,
    pub(in crate::settings) ssrf_guard: Override,
    pub(in crate::settings) fetch_budgets: Override,
    pub(in crate::settings) memory_quarantine: Override,
    pub(in crate::settings) native_web: Override,
    pub(in crate::settings) consumer_hygiene: Override,
    /// The managed-tool steering paragraph. Tab-scoped exactly like
    /// `consumer_hygiene`, which it is injected beside.
    pub(in crate::settings) tool_steering: Override,
    /// V32 Phase H. An `On` here is the per-tab way to enable the gate over its
    /// app-wide default `off` — the shape locked decision 17 expects most users
    /// to reach for first (one hardened OpenCode tab, everything else as it was).
    pub(in crate::settings) opencode_native_gate: Override,
}

impl TabInjectionOverrides {
    /// **The row a newly created AI tab gets** (V39): every tab-scoped cell
    /// explicitly `Off`.
    ///
    /// The user decision behind it: L1 and every L2 ship on, and the per-tab row
    /// is where protection is actually turned on, from the tab's shield badge —
    /// so a new tab starts with nothing engaged and the user opts in per control
    /// per tab, without opening Settings at all.
    ///
    /// Written out cell by cell rather than looped over [`Feature::ALL`]: the
    /// struct's fields are what a settings file carries, and a constructor that
    /// enumerates them fails to compile when a tab-scoped feature is added —
    /// which is the moment someone has to decide whether the new control joins
    /// the all-off baseline.
    ///
    /// Deliberately **not** `Default` — see the type docs.
    pub(in crate::settings) fn all_off() -> Self {
        Self {
            taint_latch: Override::Off,
            spotlighting: Override::Off,
            detection: Override::Off,
            ssrf_guard: Override::Off,
            fetch_budgets: Override::Off,
            memory_quarantine: Override::Off,
            native_web: Override::Off,
            consumer_hygiene: Override::Off,
            tool_steering: Override::Off,
            opencode_native_gate: Override::Off,
        }
    }

    /// This row's cell for `feature`, or [`Override::Inherit`] for a feature
    /// that has no tab scope — the honest answer, since there is no cell to
    /// read and inheriting is what "no override" means.
    pub(in crate::settings) fn get(&self, feature: Feature) -> Override {
        match feature {
            Feature::TaintLatch => self.taint_latch,
            Feature::Spotlighting => self.spotlighting,
            Feature::Detection => self.detection,
            Feature::SsrfGuard => self.ssrf_guard,
            Feature::FetchBudgets => self.fetch_budgets,
            Feature::MemoryQuarantine => self.memory_quarantine,
            Feature::NativeWeb => self.native_web,
            Feature::ConsumerHygiene => self.consumer_hygiene,
            Feature::ToolSteering => self.tool_steering,
            Feature::OpencodeNativeGate => self.opencode_native_gate,
            Feature::Canary | Feature::TerminalEscapeHygiene => Override::Inherit,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Set one cell. `None` for a feature with no tab scope — the caller is
    /// asking for a cell that does not exist, and silently dropping the write
    /// would look like it worked.
    ///
    /// `pub(in crate::settings)` like the cells themselves: an L3 write from an
    /// enforcement site is as wrong as an L3 read. Tests outside the boundary go
    /// through [`Settings::set_tab_override_for_test`].
    pub(in crate::settings) fn set(&mut self, feature: Feature, value: Override) -> Option<()> {
        match feature {
            Feature::TaintLatch => self.taint_latch = value,
            Feature::Spotlighting => self.spotlighting = value,
            Feature::Detection => self.detection = value,
            Feature::SsrfGuard => self.ssrf_guard = value,
            Feature::FetchBudgets => self.fetch_budgets = value,
            Feature::MemoryQuarantine => self.memory_quarantine = value,
            Feature::NativeWeb => self.native_web = value,
            Feature::ConsumerHygiene => self.consumer_hygiene = value,
            Feature::ToolSteering => self.tool_steering = value,
            Feature::OpencodeNativeGate => self.opencode_native_gate = value,
            Feature::Canary | Feature::TerminalEscapeHygiene => return None,
        }
        Some(())
    }
}

/// The `offload-worker` pseudo-scope's L3 row, stored on
/// [`InjectionSettings`] because the worker has no tab config to hang it off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerInjectionOverrides {
    pub(in crate::settings) taint_latch: Override,
    pub(in crate::settings) spotlighting: Override,
    pub(in crate::settings) detection: Override,
    pub(in crate::settings) ssrf_guard: Override,
    pub(in crate::settings) fetch_budgets: Override,
    pub(in crate::settings) canary: Override,
}

impl WorkerInjectionOverrides {
    /// This row's cell for `feature`, or [`Override::Inherit`] for a feature
    /// with no worker scope.
    pub(in crate::settings) fn get(&self, feature: Feature) -> Override {
        match feature {
            Feature::TaintLatch => self.taint_latch,
            Feature::Spotlighting => self.spotlighting,
            Feature::Detection => self.detection,
            Feature::SsrfGuard => self.ssrf_guard,
            Feature::FetchBudgets => self.fetch_budgets,
            Feature::Canary => self.canary,
            Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
            | Feature::ToolSteering
            | Feature::OpencodeNativeGate
            | Feature::TerminalEscapeHygiene => Override::Inherit,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Set one cell. `None` for a feature with no worker scope (see
    /// [`TabInjectionOverrides::set`]).
    pub(in crate::settings) fn set(&mut self, feature: Feature, value: Override) -> Option<()> {
        match feature {
            Feature::TaintLatch => self.taint_latch = value,
            Feature::Spotlighting => self.spotlighting = value,
            Feature::Detection => self.detection = value,
            Feature::SsrfGuard => self.ssrf_guard = value,
            Feature::FetchBudgets => self.fetch_budgets = value,
            Feature::Canary => self.canary = value,
            Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
            | Feature::ToolSteering
            | Feature::OpencodeNativeGate
            | Feature::TerminalEscapeHygiene => return None,
        }
        Some(())
    }
}

// ── Resolution ─────────────────────────────────────────────────────────────

/// Which level produced the answer. Exists because with three levels, "why is
/// this tab not latching?" must be answerable without reading code — the same
/// every-signal-needs-a-consumer discipline, applied to configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecidedBy {
    /// L1: the global master is off. Nothing below it was consulted.
    Global,
    /// L2: the scope inherits, so the app-wide per-feature flag decided.
    Feature,
    /// L3: this scope states its own answer.
    ///
    /// At [`Scope::UnknownCaller`] it means something slightly wider — "a
    /// narrower scope's `On` is being honoured here", the identity-less
    /// elevation on [`decide`] (#48, N-1). That scope has no cell of its own, so
    /// there is no other reading of `scope` available there, and the honest
    /// alternative ([`DecidedBy::Feature`]) would claim L2 said `on` when it
    /// said `off`. [`Scope::AppWide`] never produces this value.
    Scope,
}

/// One resolved control: the value in force and the level that set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub effective: bool,
    pub decided_by: DecidedBy,
}

/// The single source of truth for every V32 enforcement site.
///
/// See the module docs for the locked rule — L1 short-circuits, then L3, then
/// L2 — which is unchanged. Three subtleties in the code below:
///
/// - the L1 short-circuit applies to the features L1 is *for*
///   ([`Feature::master_gated`]). [`Feature::ToolSteering`] is not one: it is a
///   token-efficiency nudge, and the master switch means "reduce my security
///   posture". It resolves through the same L3 → L2 path with L1 simply not
///   consulted, so it never reports [`DecidedBy::Global`];
/// - [`Feature::NativeWeb`]'s L2 is derived from the tri-mode rather than stored
///   as a boolean ([`native_web_l2`]);
/// - at [`Scope::UnknownCaller`] an L3 `On` stated by **any** configured tab is
///   honoured (#48, N-1). That scope is the second of the two questions one
///   `Scope::App` used to answer — "what applies to a call that sent no
///   `--tab`" — and it has a tab behind it that we simply cannot name. Only `On`
///   travels up: an L3 `Off` stays where the user put it, so this can never
///   remove protection, only add it to a caller with no identity. It never
///   applies to a scope that HAS an answer (a known tab, the worker) and never
///   to [`Scope::AppWide`], which is the *first* question — the app-wide
///   baseline, L1 ∧ L2, borrowing nobody's answer (#48, F-35). So the locked
///   resolution order for a known scope does not move.
pub fn decide(feature: Feature, scope: Scope<'_>, s: &Settings) -> Decision {
    let inj = &s.offload.injection;
    // L1 short-circuits — for every feature the master switch is ALLOWED to
    // close ([`Feature::master_gated`]). The one that is not (managed-tool
    // steering, a token nudge rather than a containment control) falls straight
    // through to the unchanged L3 → L2 path below, so an install with
    // protection off keeps it.
    if feature.master_gated() && !inj.protection {
        return Decision {
            effective: false,
            decided_by: DecidedBy::Global,
        };
    }
    match scope_override(feature, scope, s) {
        Override::On => Decision {
            effective: true,
            decided_by: DecidedBy::Scope,
        },
        Override::Off => Decision {
            effective: false,
            decided_by: DecidedBy::Scope,
        },
        // N-1: the identity-less scope has no cell of its own, so this is where
        // its reading gets its answer, BEFORE falling through to L2. F-35: this
        // arm is the entire behavioural difference between the two app-level
        // variants — `AppWide` falls straight through to L2.
        Override::Inherit
            if matches!(scope, Scope::UnknownCaller) && any_tab_override_on(feature, s) =>
        {
            Decision {
                effective: true,
                decided_by: DecidedBy::Scope,
            }
        }
        Override::Inherit => Decision {
            effective: feature_l2(feature, s),
            decided_by: DecidedBy::Feature,
        },
    }
}

/// Whether ANY configured AI tab states an L3 `On` for `feature` — the
/// identity-less elevation described on [`Scope::UnknownCaller`] and
/// [`Scope::for_tab`] (#48, N-1).
///
/// Tabs only, deliberately: the offload worker is never the caller behind an
/// identity-less consumer call (it resolves through [`Scope::OffloadWorker`],
/// which it always has), so folding its row in would raise protection for a
/// population it does not describe.
///
/// A feature with no per-tab row can never match — [`TabInjectionOverrides::get`]
/// returns `Inherit` for those — so an app-only control (terminal escape
/// hygiene) is untouched by this, and with it [`protection_reduced`]'s app-scope
/// pass, which only ever looks at app-only controls.
fn any_tab_override_on(feature: Feature, s: &Settings) -> bool {
    s.tabs.iter().any(|t| match t {
        TabConfig::AiTool(c) => c.injection_overrides.get(feature) == Override::On,
        _ => false,
    })
}

/// [`decide`], boolean only. The form every enforcement site calls.
pub fn effective(feature: Feature, scope: Scope<'_>, s: &Settings) -> bool {
    decide(feature, scope, s).effective
}

/// The L3 cell for `feature` at `scope`, or [`Override::Inherit`] when the scope
/// has no row for it (an app scope, an unknown tab id, a worker-only feature
/// asked about a tab).
///
/// # An unknown tab id inherits, and that is unchanged by V39
///
/// A `Scope::Tab` whose id matches no configured AI tab falls to
/// `unwrap_or_default()` — an all-`Inherit` row — so it resolves at L2. V39
/// makes a *created* tab's row all-`Off` ([`TabInjectionOverrides::all_off`]),
/// and it is worth saying explicitly that this case did **not** follow it: an
/// unknown id is a tab cImp cannot describe (a stale registry entry, a caller
/// naming an id that has since been deleted), not a tab that shipped with the
/// new baseline. Resolving it to `Off` would silently disarm a caller we failed
/// to identify, which is the fail-open-into-unprotected direction
/// [`Scope::for_tab`] refuses; resolving it at L2 keeps the app-wide answer,
/// which is the same thing every other unidentified caller gets.
fn scope_override(feature: Feature, scope: Scope<'_>, s: &Settings) -> Override {
    match scope {
        // Neither app-level scope has a cell: the baseline has nothing to
        // override, and the identity-less caller's elevation is `decide`'s, not
        // a stored value (#48, F-35).
        Scope::AppWide | Scope::UnknownCaller => Override::Inherit,
        Scope::OffloadWorker => s.offload.injection.worker.get(feature),
        Scope::Tab { tab, .. } => s
            .tabs
            .iter()
            .find_map(|t| match t {
                TabConfig::AiTool(c) if c.id == tab => Some(c.injection_overrides),
                _ => None,
            })
            .unwrap_or_default()
            .get(feature),
    }
}

/// The app-wide L2 answer for `feature`.
///
/// **This function and [`InjectionSettings`]'s declaration are the only places
/// in the crate that read these raw fields** — and since #44 that is enforced by
/// their `pub(in crate::settings)` visibility rather than watched by a scan.
fn feature_l2(feature: Feature, s: &Settings) -> bool {
    let inj = &s.offload.injection;
    match feature {
        Feature::TaintLatch => inj.taint_latch_enabled,
        Feature::Spotlighting => inj.spotlighting_enabled,
        Feature::Detection => inj.detection_enabled,
        Feature::SsrfGuard => inj.ssrf_guard_enabled,
        Feature::FetchBudgets => inj.fetch_budgets_enabled,
        Feature::Canary => inj.canary_enabled,
        Feature::MemoryQuarantine => inj.memory_quarantine_enabled,
        Feature::ConsumerHygiene => inj.consumer_hygiene_enabled,
        Feature::ToolSteering => inj.tool_steering_enabled,
        Feature::OpencodeNativeGate => inj.opencode_native_gate_enabled,
        Feature::TerminalEscapeHygiene => inj.terminal_escape_hygiene_enabled,
        // No boolean of its own — the tri-mode IS the L2 (module docs).
        Feature::NativeWeb => native_web_l2(s) != NativeWebMode::Off,
    }
}

// ── Resolved-value helpers (the only other legal readers) ──────────────────

/// V32 Phase F: what cImp does about the harness's OWN web tools.
///
/// Moved here in Phase G from `tabs::config` (which re-exports it under its old
/// name) so that the raw `native_web_visibility` string has exactly one reader,
/// beside every other raw V32 field. The mode is the feature's L2, so the parse
/// and the hierarchy have to live together or they drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeWebMode {
    /// Nothing injected: no beacon hook, no permission denial. Pre-V32.
    Off,
    /// Report-only beacons that engage the tab's EXTERNAL latch. Never deny.
    Sensor,
    /// The native web tools are refused by the harness itself.
    Deny,
}

impl NativeWebMode {
    /// Parse the settings string. An unrecognized value reads as
    /// [`Sensor`](Self::Sensor) — the default — for the same reason C3's
    /// `Mode::parse` falls back to `check`: a typo must neither blind the latch
    /// (`off`) nor silently take a tool away from a working tab (`deny`).
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "off" => NativeWebMode::Off,
            "deny" => NativeWebMode::Deny,
            _ => NativeWebMode::Sensor,
        }
    }

    /// The canonical wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            NativeWebMode::Off => "off",
            NativeWebMode::Sensor => "sensor",
            NativeWebMode::Deny => "deny",
        }
    }
}

/// The app-wide mode as stored (the feature's L2 input).
fn native_web_l2(s: &Settings) -> NativeWebMode {
    NativeWebMode::parse(&s.offload.native_web_visibility)
}

/// The native-web mode **in force for `scope`** — the value every spawn site
/// reads.
///
/// Composition with the hierarchy, in full:
/// - feature resolves off ⇒ [`NativeWebMode::Off`], whatever the mode says;
/// - feature resolves on and the stored mode is not `off` ⇒ the stored mode;
/// - feature resolves on and the stored mode IS `off` (only reachable via an L3
///   `On`) ⇒ [`NativeWebMode::Sensor`], the mode's own default. See the module
///   docs for why `sensor` and not `deny`.
pub fn native_web_mode(s: &Settings, scope: Scope<'_>) -> NativeWebMode {
    if !effective(Feature::NativeWeb, scope, s) {
        return NativeWebMode::Off;
    }
    match native_web_l2(s) {
        NativeWebMode::Off => NativeWebMode::Sensor,
        other => other,
    }
}

/// The EXTERNAL call/byte caps in force for `scope` (locked decision 11's
/// numerics, gated by [`Feature::FetchBudgets`]).
///
/// A disabled feature returns `0`/`0`, which is already the "no cap" spelling
/// both halves of `outbound::Budget::exhausted` understand — so turning budgets
/// off needs no second code path at the two gates, and the existing numerics
/// keep their meaning as the tuning knobs above the switch.
pub fn budget_limits(s: &Settings, scope: Scope<'_>) -> crate::offload::outbound::BudgetLimits {
    if !effective(Feature::FetchBudgets, scope, s) {
        return crate::offload::outbound::BudgetLimits {
            max_calls: 0,
            max_bytes: 0,
        };
    }
    crate::offload::outbound::BudgetLimits {
        max_calls: s.offload.external_fetch_max_calls,
        max_bytes: s.offload.external_fetch_max_bytes,
    }
}

/// The detection layers in force for `scope`.
///
/// The parent switch is checked first and wins: with [`Feature::Detection`] off
/// both layers are off *regardless of the two per-layer sub-toggles*, which stay
/// exactly what they were (the layer selection inside an enabled surface).
pub fn detection_config(s: &Settings, scope: Scope<'_>) -> crate::offload::detection::Config {
    detection_layers(s, effective(Feature::Detection, scope, s))
}

/// [`detection_config`], for the *"armed anywhere in this process"* question
/// (#48, F-35).
///
/// **Reporting only. Never a gate** — see [`armed_anywhere`], whose caveats all
/// apply. It exists rather than leaving its two callers to write
/// `armed_anywhere(Feature::Detection, s) && …signature_enabled` because that
/// second conjunct is a raw settings field, which is a privacy error outside
/// this module (the no-raw-reads invariant in the module docs) and would compose
/// the per-layer sub-toggle a second time.
pub fn detection_config_anywhere(s: &Settings) -> crate::offload::detection::Config {
    detection_layers(s, armed_anywhere(Feature::Detection, s))
}

/// The layer composition both readings share, so the parent switch and the two
/// sub-toggles meet in exactly one place whichever question asked.
fn detection_layers(s: &Settings, parent_on: bool) -> crate::offload::detection::Config {
    if !parent_on {
        return crate::offload::detection::Config {
            signature: false,
            classifier: false,
            classifier_threshold: s.offload.detection_classifier_threshold,
        };
    }
    crate::offload::detection::Config {
        signature: s.offload.detection_signature_enabled,
        classifier: s.offload.detection_classifier_enabled,
        classifier_threshold: s.offload.detection_classifier_threshold,
    }
}

/// Whether `feature` is in force in **any** scope this process has: app-wide, on
/// any configured AI tab, or in the offload worker (#48, F-35, locked decision
/// 36).
///
/// # Reporting only. Never a gate.
///
/// An enforcement site asking *"may I do this?"* wants [`effective`] for the
/// scope it is actually running in. This predicate is true when a control is
/// armed for **somebody else**, so gating on it would refuse a call on another
/// scope's behalf — which is why the third question got a function and
/// deliberately **not** a `Scope::Anywhere` variant: every helper keyed on
/// [`Scope`] would have accepted that variant, and `native_web_mode(s,
/// Scope::Anywhere)` would take a tool away from one tab because a *different*
/// tab was hardened.
///
/// # Its consumers, and why they are the right ones
///
/// The two signals that describe the user's own `detection/rules.d/local/`
/// directory: `updater::broken_local_rules` and `signature::advisor_signal`.
/// That directory is **one directory shared by every scope**, so "is anyone
/// scanning with these files" is genuinely the question they ask — and they
/// asked it at the app scope until F-35, which meant a user who narrowed
/// detection to the offload worker (L2 `off`, `worker.detection = on`) was told
/// **nothing** while the worker screened every fetched page with rules of theirs
/// that had failed to compile.
///
/// # What it is NOT
///
/// It is not `updates_enabled`'s question. M-21 settled that one: the updater
/// stays app-scoped and one worker override does not start it. Repointing
/// `updates_enabled` here would also collapse `worker_only_detection` — defined
/// as `!updates_enabled && effective(Detection, OffloadWorker)` — to a permanent
/// `false`, killing the frontend branch that renders the worker-only sentence.
///
/// # Composition
///
/// Built from [`decide`] rather than from raw fields, so L1 still
/// short-circuits: with the master off this is `false` for every feature, at
/// every scope, like everything else. The union is
/// [`Scope::UnknownCaller`] (L1 ∧ (L2 ∨ any tab's `On`)) with
/// [`Scope::OffloadWorker`] (which contributes the worker's own row) — the one
/// place in the crate where the worker's row is deliberately folded into an
/// identity-less answer, six lines from the elevation that must never do it
/// (`any_tab_override_on`, *"tabs only, deliberately"*).
pub fn armed_anywhere(feature: Feature, s: &Settings) -> bool {
    armed_outside_the_worker(feature, s) || effective(feature, Scope::OffloadWorker, s)
}

/// Whether `feature` is in force for any caller that is **not** the offload
/// worker — the app-wide baseline, plus every L3 `On` a configured AI tab
/// states (#48, F-38, locked decision 41).
///
/// # Why this exists at all: a name for a question, not a new resolution
///
/// This is `effective(feature, Scope::UnknownCaller, s)` and nothing else, so it
/// decides nothing new. What it adds is a **name**. Locked decision 36 split
/// `Scope::App` into [`Scope::AppWide`] and [`Scope::UnknownCaller`] because one
/// name standing in for a question it did not ask had already produced two
/// defects (M-21, F-35). Two sites then kept `UnknownCaller` correctly and read
/// wrongly: [`updates_enabled`](crate::offload::detection::updater::updates_enabled)
/// does not ask *"what applies to a caller with no identity?"* — it asks
/// *"is the one shared detection bundle on disk worth keeping current?"*, whose
/// answer is *"yes iff detection is armed for somebody other than the worker"*.
/// Leaving the `UnknownCaller` spelling at such a site recreates the exact
/// condition F-35 was about, one reader further on.
///
/// **The other site keeps its spelling, deliberately.**
/// `loopback::injection_status`'s `app` row reports a whole feature *matrix*
/// (`report(settings, Scope::UnknownCaller)`), not a predicate, so it has no use
/// for this function — and decision 41 declined to move it for a second reason:
/// its `decided_by`/`override_value` pair is the only observation point
/// live-verify's N-1 box has.
///
/// # Why the worker is the excluded half, and why that is a CONTRACT
///
/// [`Scope::UnknownCaller`]'s elevation is *"tabs only, deliberately"*: it exists
/// for a call that arrived without its tab identity, and the worker is never that
/// caller — it always resolves through [`Scope::OffloadWorker`]. So the identity-
/// less answer already **is** the non-worker answer, and this function is the
/// complement half of [`armed_anywhere`], which is now literally composed from
/// it: `armed_anywhere = armed_outside_the_worker ∨ the worker's own row`.
///
/// That decomposition is why `updater::worker_only_detection` — defined as
/// `!updates_enabled ∧ effective(_, OffloadWorker)` — reads as *"armed anywhere,
/// but not outside the worker"*, and why repointing `updates_enabled` at
/// [`armed_anywhere`] would collapse it to a permanent `false` and orphan the
/// Svelte branch gated on it. M-21 rejected that; this naming does not reopen it.
///
/// # Reporting-ish, like its sibling
///
/// An enforcement site asking *"may I do this?"* still wants [`effective`] for
/// the scope it is running in. This one is for the surfaces whose subject is the
/// process rather than the caller — a shared file on disk, a shared directory of
/// rules — and it can be true while every individual tab has an L3 `Off`,
/// because an identity-less call would still be screened.
pub fn armed_outside_the_worker(feature: Feature, s: &Settings) -> bool {
    effective(feature, Scope::UnknownCaller, s)
}

// ── Spawn signature + introspection ───────────────────────────────────────

/// Which AI consumer a spawn signature is being built for.
///
/// The split matches how `tabs::config` already divides the launch path:
/// `build_pre_args` is Claude-only, `build_opencode_config` /
/// `write_opencode_plugin` take everything else. So the two consumers' tab sets
/// PARTITION the AI tabs — no tab belongs to neither, which is what keeps the
/// per-consumer signatures from dropping a row between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consumer {
    Claude,
    Opencode,
}

impl Consumer {
    /// The consumer a tab whose command is `command` launches, or `None` when
    /// the command names no registered harness.
    ///
    /// **V40 Phase A made this fallible** (locked decision 2). It used to answer
    /// `Opencode` for anything that was not Claude, so a tab pointed at a third
    /// CLI had OpenCode's injection hierarchy resolved for it — the wrong
    /// spawn-baked flags, silently. A tab that is not a registered harness has
    /// no consumer, and every caller now says so.
    pub fn for_command(command: &str) -> Option<Self> {
        let id = crate::harness::HarnessId::from_command(command)?.id()?;
        Self::from_agent(id)
    }

    /// The consumer for a CHP `agent` token, or `None`.
    pub fn from_agent(agent: &str) -> Option<Self> {
        match agent {
            "claude" => Some(Consumer::Claude),
            "opencode" => Some(Consumer::Opencode),
            _ => None,
        }
    }

    /// This consumer in the normalized agent vocabulary the rest of the crate
    /// keys on (`tabs::config::tab_consumer`,
    /// `ToolPluginsSettings::commands_exposed_to`, the latch registry). One
    /// spelling, so a per-consumer settings lookup cannot disagree with the
    /// scope the same tab resolves through.
    pub fn agent(self) -> &'static str {
        match self {
            Consumer::Claude => "claude",
            Consumer::Opencode => "opencode",
        }
    }

    /// Whether `feature`'s spawn-baked value reaches this consumer's launch at
    /// all.
    ///
    /// Exhaustive, so a new [`Feature`] cannot be added without an answer here —
    /// and the answer defaults to nothing: a control that reaches neither
    /// consumer would simply never raise a hint, which is why the match names
    /// every variant rather than falling through.
    fn reads(self, feature: Feature) -> bool {
        match feature {
            // Claude reads the mode in `build_pre_args` (the beacon hook, and
            // `permissions.deny` in `deny` mode); OpenCode bakes it into the
            // plugin's beacon flag and its pinned permission block.
            Feature::NativeWeb => true,
            // The hygiene paragraph is Claude's `--append-system-prompt` and
            // OpenCode's managed instructions file; the pinned permission block
            // is OpenCode's alone.
            Feature::ConsumerHygiene => true,
            // The steering paragraph rides the SAME channel as the hygiene one,
            // for both consumers — `compose_capability_guidance` composes them
            // one after the other.
            Feature::ToolSteering => true,
            // Phase H's gate exists only inside the generated OpenCode plugin.
            // This is the row the shared blob was nagging Claude tabs about.
            Feature::OpencodeNativeGate => self == Consumer::Opencode,
            // #48 (M-3): BOTH consumers bake it. `fact_promotion_block` is
            // called for `claude` and for `opencode` alike (the `agent`
            // parameter), landing in `--append-system-prompt` on one side and
            // the managed instructions file on the other — so an L2 flip owes
            // both a hint, and an L3 flip owes the tab's own consumer one.
            Feature::Spotlighting => true,
            // Live features never reach a spawn signature at all — filtered out
            // by `Feature::spawn_baked` before this is asked — but naming them
            // keeps the match total.
            Feature::TaintLatch
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary
            | Feature::MemoryQuarantine
            | Feature::TerminalEscapeHygiene => false,
        }
    }
}

/// The hierarchy's contribution to `tabs::config::spawn_inject_sig`, **for one
/// consumer**.
///
/// Every level of every **spawn-baked** feature ([`Feature::spawn_baked`]) that
/// `consumer` actually reads, for every AI tab that IS a `consumer` tab: the
/// master switch, the app-wide L2 inputs, and each tab's resolved values. A flip
/// at any of the three levels moves this, so the user gets the restart hint a
/// spawn-baked change owes them.
///
/// **Per consumer since #48 (F-x).** Both consumer objects used to embed the
/// identical blob, so an OpenCode-only flip — `opencode_native_gate`, or a
/// native-web override on the OpenCode tab — marked Claude tabs dirty too and
/// nagged them to restart for a change that could not reach them. The feature
/// that introduced the rule ("a restart nag for a change that needs no restart
/// is how a hint stops being read") was violating it. Two filters do the work,
/// and nothing else changed: which TABS contribute a row, and which FEATURES
/// each row (and the `l2` array) carries.
///
/// Live features are deliberately absent: they take effect on the next call, and
/// a restart nag for a change that needs no restart is how a hint stops being
/// read.
///
/// It lives here rather than in `tabs::config` because it is the only other
/// thing that has to look at the raw switches, and the no-raw-reads invariant is
/// worth more than the locality. Since #44 that is not a preference: the fields
/// are `pub(in crate::settings)`, so `tabs::config` could not read them anyway.
///
/// The resolved native-web MODE rides alongside the switches deliberately:
/// `sensor` and `deny` both resolve the feature "on" but launch a tab very
/// differently, so a signature built from booleans alone would miss a mode
/// change. [`Feature::ToolSteering`]'s `tool_plugins.expose_commands_*` flag
/// rides for the same reason and is scoped the same way — see the comment at
/// its site below, including why the tool REGISTRY deliberately stays out.
pub fn spawn_sig(s: &Settings, consumer: Consumer) -> serde_json::Value {
    // The spawn-baked features this consumer reads, in `Feature::ALL` order.
    // Driven by `Feature::spawn_baked` rather than by a hand-written pair, so a
    // future spawn-baked control gets its restart hint by declaring itself —
    // not by someone remembering this function.
    let features: Vec<Feature> = Feature::ALL
        .iter()
        .copied()
        .filter(|f| f.spawn_baked() && consumer.reads(*f))
        .collect();
    let consumer_tabs: Vec<_> = s
        .tabs
        .iter()
        .filter_map(|t| match t {
            TabConfig::AiTool(c) if Consumer::for_command(&c.command) == Some(consumer) => Some(c),
            _ => None,
        })
        .collect();
    let rows: Vec<serde_json::Value> = consumer_tabs
        .iter()
        .map(|c| {
            let scope = Scope::tab_only(&c.id);
            let resolved: Vec<serde_json::Value> = features
                .iter()
                .map(|f| serde_json::json!([f.key(), effective(*f, scope, s)]))
                .collect();
            serde_json::json!([c.id, native_web_mode(s, scope).as_str(), resolved])
        })
        .collect();
    // [`Feature::ToolSteering`]'s rendered paragraph has a second input that is
    // not a switch in this hierarchy: `tool_plugins.expose_commands_*` decides
    // whether the `run_command` half of it is written at all. Same reasoning as
    // the native-web MODE below — a signature built from the feature booleans
    // alone would miss it, and the paragraph is baked at spawn, so a flip owes
    // the user a restart hint.
    //
    // Two properties this shape buys, both deliberate:
    //
    // * **`None` when no tab of this consumer actually renders the paragraph.**
    //   With steering resolved off everywhere the flag cannot change what a
    //   fresh tab writes, and nagging for it is how a restart hint stops being
    //   read. A later `On` at any level moves the `l2`/`tabs` entries anyway, so
    //   nothing is lost by staying quiet until then.
    // * **The FLAG, never the registry.** Detecting a binary, enabling a plugin
    //   check or repointing a path changes the tools' own enums, which are LIVE
    //   (`graph::mcp`'s native pulse re-advertises them to a running tab). The
    //   paragraph names none of them, so none of them belongs in a spawn
    //   signature.
    let commands_exposed = consumer_tabs
        .iter()
        .any(|c| effective(Feature::ToolSteering, Scope::tab_only(&c.id), s))
        .then(|| s.tool_plugins.commands_exposed_to(consumer.agent()));
    // The app-wide L2 input for each of those features. The native-web entry is
    // the tri-mode string — that IS its L2 (the module docs' reconciliation
    // note), and it carries `sensor` vs `deny`, which a boolean would lose.
    let l2: Vec<serde_json::Value> = features
        .iter()
        .map(|f| match f {
            Feature::NativeWeb => {
                serde_json::json!([f.key(), s.offload.native_web_visibility.clone()])
            }
            _ => serde_json::json!([f.key(), feature_l2(*f, s)]),
        })
        .collect();
    serde_json::json!({
        // L1 explicitly, even though it is folded into every resolved value
        // above: a master flip on an install with no AI tabs configured must
        // still move the signature rather than compare equal to itself. Shared
        // by both consumers on purpose — L1 is the one switch that reaches
        // every launch there is.
        "master": s.offload.injection.protection,
        "l2": l2,
        "tabs": rows,
        // `null` while no tab of this consumer renders the steering paragraph —
        // see the note above.
        "commands_exposed": commands_exposed,
    })
}

/// One row of the introspection report: a feature's resolved state at a scope,
/// and which level decided it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FeatureState {
    pub feature: &'static str,
    pub label: &'static str,
    pub effective: bool,
    pub decided_by: DecidedBy,
    /// The scope's own cell, so the UI can render the tri-state control without
    /// a second round trip.
    pub override_value: &'static str,
    /// Whether this scope has a row for the feature at all. `false` rows are
    /// still reported — "this control does not apply here" is an answer the
    /// question "why is this tab not latching?" sometimes needs.
    pub in_scope: bool,
    /// V32 Phase H: this feature's L2 default ([`Feature::default_enabled`]).
    ///
    /// Published rather than mirrored in the frontend so the "is protection
    /// REDUCED here?" question has one definition. The tab badge and the
    /// status-bar chip both mean "something is off that ships on"; a
    /// default-off control that is simply off is the baseline, not a reduction,
    /// and the rule that decides which is which must not live in two languages.
    pub default_on: bool,
    /// [`Feature::spawn_baked`] — whether changing this control only takes
    /// effect on the next tab spawn.
    ///
    /// Published for the same reason as `label` and `default_on` (#48, F-y): the
    /// Settings matrix used to carry its own copy of this predicate beside its
    /// own copy of the labels and the scope rules, and #47 made every Rust
    /// mirror of the feature table a compile error while leaving that one — the
    /// only hand-maintained enumeration left — with no signal at all. The matrix
    /// now renders from this report, so a V33 control appears in Settings with
    /// its restart hint the day it is declared.
    pub spawn_baked: bool,
    /// [`Feature::master_gated`] — whether the L1 master switch reaches this
    /// control at all.
    ///
    /// Published for the same reason as `default_on`: the "is protection
    /// REDUCED here?" rule must have ONE definition. [`protection_reduced`]
    /// skips non-master-gated features (a token nudge switched off is not less
    /// protection), and `latch.ts`'s `isReducedRow` has to skip exactly the same
    /// rows — which it could not derive from `default_on`, since managed-tool
    /// steering ships ON. The Settings matrix reads it too: its L2 checkbox is
    /// disabled while the master is off, and a row the master cannot reach must
    /// stay editable and say so.
    pub master_gated: bool,
}

/// Whether `scope` carries a row for `feature` at all — the structural question
/// [`report`] answers per row and [`protection_reduced`] must ask before
/// counting a feature as switched off somewhere.
///
/// Extracted so the two cannot disagree: a feature counted "off at the app
/// scope" that in fact has no app row would light the reduced-protection
/// indicator forever.
fn feature_in_scope(feature: Feature, scope: Scope<'_>) -> bool {
    match scope {
        Scope::Tab { .. } => feature.has_tab_scope(),
        Scope::OffloadWorker => feature.has_worker_scope(),
        // Structural, so the two app-level scopes answer alike: the question is
        // "is there a row here", and neither has one for a feature that keeps
        // its cells on tabs or on the worker.
        Scope::AppWide | Scope::UnknownCaller => {
            !feature.has_tab_scope() && !feature.has_worker_scope()
        }
    }
}

/// Every feature's resolved state at one scope — what `/status`, the
/// `injection_status` IPC command and the Settings matrix all render.
pub fn report(s: &Settings, scope: Scope<'_>) -> Vec<FeatureState> {
    Feature::ALL
        .iter()
        .map(|f| {
            let d = decide(*f, scope, s);
            FeatureState {
                feature: f.key(),
                label: f.label(),
                effective: d.effective,
                decided_by: d.decided_by,
                override_value: scope_override(*f, scope, s).as_str(),
                in_scope: feature_in_scope(*f, scope),
                default_on: f.default_enabled(),
                spawn_baked: f.spawn_baked(),
                master_gated: f.master_gated(),
            }
        })
        .collect()
}

/// Whether protection is REDUCED anywhere the user can see — the master is off,
/// or any master-gated feature that ships ON resolves off at a scope that has a
/// row for it.
///
/// **Only master-gated controls count** ([`Feature::master_gated`]). V38's
/// managed-tool steering lives in this hierarchy for its switches, not because
/// it protects anything; switching it off is a token-budget choice, and a
/// security indicator that lights for it is an indicator that stops being read.
///
/// **"Reduced" is measured against each feature's default**
/// ([`Feature::default_enabled`]), not against `true`. Phase H's OpenCode native
/// gate ships off by user decision, so a fresh install must not report reduced
/// protection; and a scope that switches that gate ON has more protection than
/// the default, never less.
///
/// **A tab's own `Off` cell is the tab BASELINE and does not count** (V39). A
/// newly created AI tab ships every tab-scoped cell `Off`
/// ([`TabInjectionOverrides::all_off`]) and the user arms them from the tab's
/// shield badge, so counting those cells would raise the indicator on every tab
/// of every fresh install — the exact "an indicator nobody reads" failure the
/// `default_enabled` clause above exists to prevent, one level down.
///
/// The tab pass is **narrowed, not dropped**: a tab row that is off because L2
/// is off still counts. Dropping the pass outright would have opened a real
/// hole, because three master-gated, ships-on controls have a tab row and *no*
/// other row — memory quarantine, native-web visibility and consumer hygiene —
/// so [`Scope::AppWide`] and [`Scope::OffloadWorker`] between them can never see
/// them. Switching one of those off app-wide would then be invisible to every
/// surface, while a legacy tab (whose cells the 34 → 35 step wrote as explicit
/// `inherit`) really did lose the protection. So the filter is on *who decided*,
/// not on which scope asked: `!effective && decided_by == Scope` at a tab is the
/// user's per-tab baseline; anything else off at a tab is a reduction.
///
/// Its frontend twin `isReducedRow` in `latch.ts` takes the scope key for the
/// same reason and applies the same two-part rule.
///
/// This is the predicate behind the out-of-Settings indicator (locked decision
/// 16: "a reduced-protection state … is visible outside Settings too … so
/// protection cannot be off and forgotten"). Deliberately global rather than
/// per-tab: the status-bar surface has one pixel budget and the question it
/// answers is "is anything off right now".
/// The L1 master switch's value, for the surfaces that must show it as a switch
/// rather than as one resolved feature among ten.
///
/// A function rather than a direct field read at those call sites, because the
/// no-raw-reads invariant is only enforceable if it has no exceptions worth
/// arguing about — and since #44 there is no other option: `protection` is
/// `pub(in crate::settings)`, so this accessor is the ONLY way an outside module
/// can see the master. Its only caller outside this module is
/// `offload::loopback::injection_status`, which renders the master as a switch
/// beside the resolved features. Keep it `pub`.
///
/// **Not for gating.** An enforcement site asking "may I do this?" wants
/// [`effective`] for its own [`Feature`], which folds L1 in already; gating on
/// L1 alone answers a different question and misses L2. The detection updater's
/// scheduler did exactly that between #46 and #48 — protection on, detection
/// off, and it still made a daily request and swapped bundles for a surface
/// that was switched off.
pub fn master_enabled(s: &Settings) -> bool {
    s.offload.injection.protection
}

pub fn protection_reduced(s: &Settings) -> bool {
    if !s.offload.injection.protection {
        return true;
    }
    // `count_tab_baseline = false` is the V39 narrowing: at a TAB scope, a row
    // switched off by the tab's own cell is the baseline the tab shipped with,
    // not a reduction. Everything else about the predicate is unchanged, and the
    // two app-level scopes pass `true` because their rows have no per-tab cell
    // to be a baseline in the first place.
    let any_off = |scope: Scope<'_>, count_tab_baseline: bool| {
        Feature::ALL.iter().any(|f| {
            // Only controls the master switch is ABOUT can reduce protection:
            // managed-tool steering switched off costs tokens, not posture, and
            // counting it would raise the ⛨ chip for a preference. The frontend
            // twin (`isReducedRow` in `latch.ts`) filters on the same bit,
            // published per row as `master_gated`.
            if !(f.master_gated() && f.default_enabled() && feature_in_scope(*f, scope)) {
                return false;
            }
            let d = decide(*f, scope, s);
            !d.effective && (count_tab_baseline || d.decided_by != DecidedBy::Scope)
        })
    };
    // `AppWide`, not `UnknownCaller` (#48, F-35), and the two are provably equal
    // here: `feature_in_scope` admits only features with no tab row, and only a
    // tab row can carry the N-1 elevation.
    if any_off(Scope::AppWide, true) || any_off(Scope::OffloadWorker, true) {
        return true;
    }
    s.tabs.iter().any(|t| match t {
        TabConfig::AiTool(c) => any_off(
            Scope::Tab {
                agent: "",
                tab: &c.id,
            },
            false,
        ),
        _ => false,
    })
}

// ── Test-only writers (the boundary's other half) ─────────────────────────
//
// The switches are `pub(in crate::settings)`, so a test in `offload::*` or
// `tabs::config` that wants a feature off cannot poke the field. These helpers
// are the sanctioned way in, and they are keyed on [`Feature`] rather than on
// field names deliberately: a test cannot ask for a flag that no longer exists,
// and adding a variant to `Feature` fails to compile here until the new
// control's L2 storage is named. That is a stronger property than the field
// names the retired tripwire searched for (#44).
/// Which detection layer a test is setting — see
/// [`Settings::set_detection_layer_for_test`]. Test-only, because the two
/// layers have no run-time selector of their own: production code reads them
/// through [`detection_config`], which resolves [`Feature::Detection`] first.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetectionLayer {
    Signature,
    Classifier,
}

#[cfg(test)]
impl Settings {
    /// L1 — the global master.
    pub(crate) fn set_master_for_test(&mut self, on: bool) {
        self.offload.injection.protection = on;
    }

    /// L2 — the app-wide flag for one feature.
    ///
    /// Total over [`Feature`], including [`Feature::NativeWeb`], whose L2 is the
    /// `native_web_visibility` tri-mode rather than a boolean (the Phase G
    /// reconciliation): `false` writes `off`, `true` writes the mode's own
    /// default `sensor`. A test that needs `deny` sets the mode itself — that
    /// is a posture, not an on/off.
    pub(crate) fn set_l2_for_test(&mut self, feature: Feature, on: bool) {
        let inj = &mut self.offload.injection;
        match feature {
            Feature::TaintLatch => inj.taint_latch_enabled = on,
            Feature::Spotlighting => inj.spotlighting_enabled = on,
            Feature::Detection => inj.detection_enabled = on,
            Feature::SsrfGuard => inj.ssrf_guard_enabled = on,
            Feature::FetchBudgets => inj.fetch_budgets_enabled = on,
            Feature::Canary => inj.canary_enabled = on,
            Feature::MemoryQuarantine => inj.memory_quarantine_enabled = on,
            Feature::ConsumerHygiene => inj.consumer_hygiene_enabled = on,
            Feature::ToolSteering => inj.tool_steering_enabled = on,
            Feature::OpencodeNativeGate => inj.opencode_native_gate_enabled = on,
            Feature::TerminalEscapeHygiene => inj.terminal_escape_hygiene_enabled = on,
            Feature::NativeWeb => {
                self.offload.native_web_visibility = if on {
                    NativeWebMode::Sensor.as_str().to_string()
                } else {
                    NativeWebMode::Off.as_str().to_string()
                }
            }
        }
    }

    /// L2 for [`Feature::NativeWeb`] as a **posture** rather than an on/off.
    ///
    /// `set_l2_for_test` can only say `off`/`sensor`; `deny` is a third posture,
    /// and several `tabs::config` tests sweep all three. Added with #48, when
    /// `native_web_visibility` joined the other L2 switches behind
    /// `pub(in crate::settings)` — by the Phase G reconciliation this tri-mode
    /// IS that feature's L2, so it belongs on the same side of the boundary.
    pub(crate) fn set_native_web_mode_for_test(&mut self, mode: NativeWebMode) {
        self.offload.native_web_visibility = mode.as_str().to_string();
    }

    /// One of the two detection LAYERS beneath [`Feature::Detection`].
    ///
    /// Enum-keyed like the rest (#48): the layer selection lives *inside* an
    /// enabled surface — `detection_config` checks the feature first and wins —
    /// so these are inputs to the resolver, not switches an enforcement site may
    /// read on its own.
    pub(crate) fn set_detection_layer_for_test(&mut self, layer: DetectionLayer, on: bool) {
        match layer {
            DetectionLayer::Signature => self.offload.detection_signature_enabled = on,
            DetectionLayer::Classifier => self.offload.detection_classifier_enabled = on,
        }
    }

    /// L3 — one tab's override cell. `None` when no AI tab carries `tab_id`, or
    /// when `feature` has no per-tab row: both are a write the caller believes
    /// landed and did not, which is exactly what
    /// [`TabInjectionOverrides::set`]'s `Option` exists to say.
    pub(crate) fn set_tab_override_for_test(
        &mut self,
        tab_id: &str,
        feature: Feature,
        value: Override,
    ) -> Option<()> {
        self.tabs.iter_mut().find_map(|t| match t {
            TabConfig::AiTool(c) if c.id == tab_id => {
                Some(c.injection_overrides.set(feature, value))
            }
            _ => None,
        })?
    }

    /// L3 — the `offload-worker` pseudo-scope's override cell. `None` for a
    /// feature with no worker row (see [`WorkerInjectionOverrides::set`]).
    pub(crate) fn set_worker_override_for_test(
        &mut self,
        feature: Feature,
        value: Override,
    ) -> Option<()> {
        self.offload.injection.worker.set(feature, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings snapshot with one AI tab present, since `Settings::default()`
    /// carries an EMPTY tab list (the builtins are materialized by the load-time
    /// integrity check, not by `Default`) and a per-tab override has to have a
    /// tab to attach to.
    ///
    /// **The tab is all-`Inherit`, not the V39 shipping tab.** Almost every test
    /// below is about the RESOLUTION RULE, and a fixture whose L3 row is already
    /// all-`Off` answers every one of them "off, decided at L3" before the rule
    /// under test is reached. All-`Inherit` is also a real, common shape: it is
    /// exactly what the 34 → 35 step writes into every tab that existed before
    /// V39, i.e. what every upgraded install carries. The shipping shape has its
    /// own fixture ([`fresh_install`]) and its own tests.
    fn settings() -> Settings {
        Settings {
            tabs: vec![inheriting_tab()],
            ..Settings::default()
        }
    }

    /// [`schema::default_claude_tab`] with its L3 row reset to all-`Inherit` —
    /// the post-migration shape of a tab that predates V39. Built from the
    /// builtin rather than hand-rolled so it keeps every other field the real
    /// tab has (id, command, consumer).
    fn inheriting_tab() -> TabConfig {
        let mut tab = super::super::schema::default_claude_tab();
        if let TabConfig::AiTool(c) = &mut tab {
            c.injection_overrides = TabInjectionOverrides::default();
        }
        tab
    }

    /// A FRESH-INSTALL snapshot: the builtin tabs exactly as
    /// `schema::default_*_tab` ships them, i.e. with the V39 all-`Off` L3 row.
    fn fresh_install() -> Settings {
        Settings {
            tabs: vec![
                super::super::schema::default_claude_tab(),
                super::super::schema::default_opencode_tab(),
            ],
            ..Settings::default()
        }
    }

    fn tab_scope<'a>(id: &'a str) -> Scope<'a> {
        Scope::Tab {
            agent: "claude",
            tab: id,
        }
    }

    /// An id that exists in `Settings::default()`'s tab list, so a per-tab
    /// override written by the tests is actually found by `scope_override`.
    fn a_tab(s: &Settings) -> String {
        s.tabs
            .iter()
            .find_map(|t| match t {
                TabConfig::AiTool(c) => Some(c.id.clone()),
                _ => None,
            })
            .expect("the default settings carry at least one AI tab")
    }

    fn set_tab_override(s: &mut Settings, id: &str, feature: Feature, value: Override) {
        s.set_tab_override_for_test(id, feature, value);
    }

    /// **Relocated from the retired `injection_tripwire.rs` (#44.)**
    ///
    /// Privacy makes "no enforcement site reads a raw switch" a compile error,
    /// but it cannot say anything about the level above the fields: that every
    /// [`Feature`] the hierarchy knows about *has* L2 storage, and storage of
    /// its own. A control added to the enum and then resolved off a field it
    /// shares with another one would be perfectly private and completely
    /// broken — which is the drift the tripwire's second test watched for, one
    /// level up from the fields it scanned.
    ///
    /// The property, stated against behaviour rather than against field names:
    /// for every feature, flipping its L2 moves that feature's resolved value at
    /// [`Scope::AppWide`] and **no other feature's**. `set_l2_for_test`'s exhaustive
    /// `match` is what makes a new variant impossible to add without naming its
    /// storage; this test is what makes naming the WRONG storage fail.
    #[test]
    fn every_feature_has_a_guarded_l2_field() {
        for f in Feature::ALL {
            for on in [false, true] {
                let mut s = settings();
                s.set_l2_for_test(*f, on);
                assert_eq!(
                    feature_l2(*f, &s),
                    on,
                    "{f:?} has no L2 storage of its own — `set_l2_for_test` writes a field \
                     `feature_l2` does not read back"
                );
                // Every OTHER feature stays at its declared default: the flip
                // must not be sharing a cell with a neighbour.
                for other in Feature::ALL.iter().filter(|o| *o != f) {
                    assert_eq!(
                        feature_l2(*other, &s),
                        other.default_enabled(),
                        "flipping {f:?}'s L2 also moved {other:?} — they share storage"
                    );
                }
            }
        }
    }

    /// The migration-safety test: an untouched config resolves every feature to
    /// its DECLARED DEFAULT at every scope — i.e. exactly the pre-Phase-G
    /// behaviour, at every layer at once.
    ///
    /// "Its default", not "on", since V32 Phase H: exactly one feature ships off
    /// (locked decision 17), and this test is the place that says which — a
    /// second one appearing by accident fails here.
    #[test]
    fn an_untouched_config_resolves_every_feature_to_its_default() {
        let s = settings();
        let id = a_tab(&s);
        for f in Feature::ALL {
            for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::AppWide] {
                let d = decide(*f, scope, &s);
                assert_eq!(
                    d.effective,
                    f.default_enabled(),
                    "{f:?} at {scope:?} must resolve to its declared default"
                );
                assert_eq!(d.decided_by, DecidedBy::Feature, "{f:?}");
            }
        }
        // The default-off set, named rather than counted: adding a feature here
        // is a product decision, and it should read like one in the diff.
        //
        // **Empty since V39** — master on, every sub-protection on. It was
        // `[OpencodeNativeGate]` under locked decision 17; that opt-in moved a
        // level down (a new tab's L3 row ships all `Off`), and the machinery
        // that makes a default-off control possible stays in place so the next
        // one costs a `match` arm rather than a redesign.
        let default_off: Vec<&Feature> = Feature::ALL
            .iter()
            .filter(|f| !f.default_enabled())
            .collect();
        assert!(default_off.is_empty(), "{default_off:?} ships off");
        // And an all-inherit install does NOT report reduced protection.
        assert!(!protection_reduced(&s));
    }

    /// V39: the feature that USED to ship off, end to end through the hierarchy.
    ///
    /// Locked decision 17 shipped [`Feature::OpencodeNativeGate`] with its L2
    /// `false`, because whole-surface denial of `bash`/`read`/`edit` changes
    /// everyday tab UX. V39 keeps that judgement and relocates it: the app-wide
    /// level is on with every other sub-protection, and the opt-in is the tab's
    /// own row, which a new tab ships all-`Off`. The properties worth pinning
    /// are the ones that changed and the ones that must not have:
    ///
    /// - its L2 is now on, and the report publishes `default_on: true`;
    /// - a FRESH INSTALL still denies nothing and still reports no reduction —
    ///   the same end state decision 17 wanted, reached one level lower;
    /// - a tab that opts IN gets it, and its neighbours do not.
    #[test]
    fn the_opencode_native_gate_is_now_app_wide_on_and_opted_into_per_tab() {
        let f = Feature::OpencodeNativeGate;
        assert!(f.default_enabled(), "V39: every sub-protection ships on");
        assert!(f.spawn_baked(), "its flag is baked into the plugin");
        assert!(f.has_tab_scope());
        assert!(!f.has_worker_scope(), "the worker is not a harness");

        // A fresh install: the app-wide level is on, every tab's own cell is
        // off, so nothing is denied anywhere and nothing reads as reduced.
        let fresh = fresh_install();
        for t in &fresh.tabs {
            let TabConfig::AiTool(c) = t else { continue };
            assert!(
                !effective(f, tab_scope(&c.id), &fresh),
                "{}: a new tab denies nothing until the user opts in",
                c.id
            );
        }
        assert!(!protection_reduced(&fresh));

        // One tab opts in — the shape decision 17 always expected users to reach
        // for, now reachable from that tab's shield badge.
        let mut s = fresh_install();
        let id = a_tab(&s);
        set_tab_override(&mut s, &id, f, Override::On);
        assert!(effective(f, tab_scope(&id), &s));
        for t in &s.tabs {
            let TabConfig::AiTool(c) = t else { continue };
            if c.id == id {
                continue;
            }
            assert!(!effective(f, tab_scope(&c.id), &s), "{} was not opted in", c.id);
        }
        // …and MORE protection than the baseline is still not "reduced".
        assert!(!protection_reduced(&s));

        // The master still wins over everything, and it always counts.
        s.set_master_for_test(false);
        assert!(!effective(f, tab_scope(&id), &s));
        assert!(protection_reduced(&s), "the master switch always counts");

        // The report publishes the default so the frontend need not mirror it.
        let inheriting = settings();
        let tab = a_tab(&inheriting);
        let rows = report(&inheriting, tab_scope(&tab));
        let row = rows
            .iter()
            .find(|r| r.feature == "opencode_native_gate")
            .expect("the feature has a row");
        assert!(row.default_on);
        assert!(row.effective, "an inheriting tab takes the app-wide `on`");
        assert!(row.in_scope);
        assert_eq!(
            rows.iter().filter(|r| !r.default_on).count(),
            0,
            "no control ships off"
        );
    }

    /// **V39, decision 2: a NEWLY CREATED AI tab has every tab-scoped cell
    /// `Off`.**
    ///
    /// Stated against the builtins the app actually seeds
    /// (`schema::default_*_tab`, which is also what `ai_tool_tab_defaults` hands
    /// the Settings window's "Reset to default"), and against the FEATURE table
    /// rather than the field list — a tab-scoped control added without a cell in
    /// `all_off` fails here rather than shipping silently on.
    #[test]
    fn a_newly_created_ai_tab_ships_every_tab_scoped_control_off() {
        let s = fresh_install();
        for t in &s.tabs {
            let TabConfig::AiTool(c) = t else { continue };
            for f in Feature::ALL.iter().filter(|f| f.has_tab_scope()) {
                assert_eq!(
                    c.injection_overrides.get(*f),
                    Override::Off,
                    "{}: {f:?} must ship Off on a new tab",
                    c.id
                );
                let d = decide(*f, tab_scope(&c.id), &s);
                assert!(!d.effective, "{}: {f:?}", c.id);
                assert_eq!(d.decided_by, DecidedBy::Scope, "{}: {f:?}", c.id);
            }
            // A feature with no tab row has no cell to be off — it still
            // resolves at L2, and the report marks it out of scope.
            for f in Feature::ALL.iter().filter(|f| !f.has_tab_scope()) {
                assert_eq!(c.injection_overrides.get(*f), Override::Inherit, "{f:?}");
            }
        }
        // The whole point of the baseline: a fresh install is not "reduced".
        assert!(!protection_reduced(&s));
        // And `Override::default()` did NOT move — an absent cell in a file
        // still means "inherit", which is what keeps the 34 → 35 step honest.
        assert_eq!(Override::default(), Override::Inherit);
        for f in Feature::ALL.iter().filter(|f| f.has_tab_scope()) {
            assert_eq!(
                TabInjectionOverrides::default().get(*f),
                Override::Inherit,
                "{f:?}: the SERDE default must stay neutral"
            );
        }
    }

    /// **V39, decision 4: an unknown tab id still INHERITS.**
    ///
    /// `scope_override` falls to an all-`Inherit` row for a `Scope::Tab` whose
    /// id matches no configured AI tab, so it resolves at L2 — unchanged, and
    /// worth pinning precisely because the *created*-tab default moved to
    /// all-`Off` around it. An unknown id is a tab cImp cannot describe (a stale
    /// registry entry, a caller naming a deleted id), not a tab that shipped
    /// with the new baseline; answering `Off` for it would silently disarm a
    /// caller we merely failed to identify.
    #[test]
    fn an_unknown_tab_id_inherits_rather_than_taking_the_new_tab_baseline() {
        // Deliberately the FRESH-INSTALL fixture: its real tabs are all-`Off`,
        // so a bug that answered "the tab baseline" for an id it could not find
        // would be invisible against an all-`Inherit` fixture.
        let s = fresh_install();
        for f in Feature::ALL.iter().filter(|f| f.has_tab_scope()) {
            let d = decide(*f, tab_scope("no-such-tab"), &s);
            assert_eq!(
                d,
                Decision {
                    effective: f.default_enabled(),
                    decided_by: DecidedBy::Feature
                },
                "{f:?} at an unknown tab id must resolve at L2"
            );
            // …and it tracks L2 rather than being pinned to the default.
            let mut off = s.clone();
            off.set_l2_for_test(*f, false);
            assert!(!effective(*f, tab_scope("no-such-tab"), &off), "{f:?}");
        }
        // The report says the same thing, cell and all — a surface rendering an
        // unknown id must not show a stored `off` that is not stored anywhere.
        for row in report(&s, tab_scope("no-such-tab"))
            .iter()
            .filter(|r| r.in_scope)
        {
            assert_eq!(row.override_value, "inherit", "{}", row.feature);
        }
    }

    /// The locked resolution table, exhaustively: every (L1, L2, L3) triple for
    /// a feature that has a plain boolean L2.
    #[test]
    fn the_resolution_truth_table_is_exhaustively_locked() {
        let id = {
            let s = settings();
            a_tab(&s)
        };
        for l1 in [true, false] {
            for l2 in [true, false] {
                for (l3, l3_name) in [
                    (Override::Inherit, "inherit"),
                    (Override::On, "on"),
                    (Override::Off, "off"),
                ] {
                    let mut s = settings();
                    s.offload.injection.protection = l1;
                    s.offload.injection.taint_latch_enabled = l2;
                    set_tab_override(&mut s, &id, Feature::TaintLatch, l3);
                    let d = decide(Feature::TaintLatch, tab_scope(&id), &s);
                    let (want, by) = match (l1, l3) {
                        (false, _) => (false, DecidedBy::Global),
                        (true, Override::On) => (true, DecidedBy::Scope),
                        (true, Override::Off) => (false, DecidedBy::Scope),
                        (true, Override::Inherit) => (l2, DecidedBy::Feature),
                    };
                    assert_eq!(
                        d,
                        Decision {
                            effective: want,
                            decided_by: by
                        },
                        "L1={l1} L2={l2} L3={l3_name}"
                    );
                }
            }
        }
    }

    /// The two edges the rule exists to state: an L3 `On` re-enables past an L2
    /// `off`, and NOTHING re-enables past an L1 `off`.
    #[test]
    fn scope_on_beats_feature_off_and_global_off_beats_everything() {
        let mut s = settings();
        let id = a_tab(&s);
        s.offload.injection.taint_latch_enabled = false;
        set_tab_override(&mut s, &id, Feature::TaintLatch, Override::On);
        assert!(effective(Feature::TaintLatch, tab_scope(&id), &s));
        // A second tab with no override stays off — that is what "app-wide L2"
        // means, and it is the exact shape live-verify 14 checks.
        assert!(!effective(
            Feature::TaintLatch,
            tab_scope("some-other-tab"),
            &s
        ));

        s.offload.injection.protection = false;
        for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::AppWide] {
            assert_eq!(
                decide(Feature::TaintLatch, scope, &s),
                Decision {
                    effective: false,
                    decided_by: DecidedBy::Global
                }
            );
        }
        // And with the master off every feature the master is ABOUT is off, at
        // every scope — locked decision (b). The exception is named rather than
        // skipped silently: a second one appearing by accident fails here.
        for f in Feature::ALL.iter().filter(|f| f.master_gated()) {
            for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::AppWide] {
                assert!(!effective(*f, scope, &s), "{f:?} at {scope:?}");
            }
        }
        let ungated: Vec<_> = Feature::ALL.iter().filter(|f| !f.master_gated()).collect();
        assert_eq!(ungated, vec![&Feature::ToolSteering]);
    }

    /// **Locked decision: the managed-tool steering nudge is NOT master-gated.**
    ///
    /// The live finding this closes: a project with `protection: false` in its
    /// `.cimp/config.json` lost the `run_check` / `run_command` paragraph, so a
    /// SECURITY escape hatch silently made the session more expensive. The
    /// master switch means "reduce my security posture"; a token nudge is not
    /// posture.
    ///
    /// (a) With L1 off the feature resolves through the unchanged L3 → L2 path,
    /// and it never reports [`DecidedBy::Global`] — there is no level above L2
    /// for it to name.
    #[test]
    fn tool_steering_is_not_closed_by_the_master_switch() {
        let f = Feature::ToolSteering;
        assert!(!f.master_gated());
        let mut s = settings();
        let id = a_tab(&s);
        s.set_master_for_test(false);

        // L2 default: on, decided by the feature level.
        for scope in [tab_scope(&id), Scope::AppWide, Scope::UnknownCaller] {
            assert_eq!(
                decide(f, scope, &s),
                Decision {
                    effective: true,
                    decided_by: DecidedBy::Feature
                },
                "{scope:?}"
            );
        }

        // L2 off, through the sanctioned test-only writer.
        let mut off = s.clone();
        off.set_l2_for_test(f, false);
        assert_eq!(
            decide(f, tab_scope(&id), &off),
            Decision {
                effective: false,
                decided_by: DecidedBy::Feature
            }
        );

        // L3 Off wins over the L2 `on`…
        let mut l3_off = s.clone();
        set_tab_override(&mut l3_off, &id, f, Override::Off);
        assert_eq!(
            decide(f, tab_scope(&id), &l3_off),
            Decision {
                effective: false,
                decided_by: DecidedBy::Scope
            }
        );
        // …and only for that tab.
        assert!(effective(f, tab_scope("some-other-tab"), &l3_off));

        // …and L3 On wins over an L2 `off`.
        let mut l3_on = off.clone();
        set_tab_override(&mut l3_on, &id, f, Override::On);
        assert_eq!(
            decide(f, tab_scope(&id), &l3_on),
            Decision {
                effective: true,
                decided_by: DecidedBy::Scope
            }
        );
    }

    /// (c) The reduced-protection predicate ignores managed-tool steering
    /// entirely — with protection ON and the nudge off at any level, nothing is
    /// reduced. Its frontend twin (`isReducedRow` in `latch.ts`) filters on the
    /// `master_gated` bit the report publishes, and (d) below pins that the bit
    /// says what [`Feature::master_gated`] says.
    #[test]
    fn tool_steering_never_counts_as_reduced_protection() {
        let f = Feature::ToolSteering;
        let base = settings();
        assert!(!protection_reduced(&base));
        assert!(f.default_enabled(), "it ships on, so `default_on` cannot filter it");

        let mut l2 = settings();
        l2.set_l2_for_test(f, false);
        assert!(
            !protection_reduced(&l2),
            "a token nudge switched off app-wide is not less protection"
        );

        let mut l3 = settings();
        let id = a_tab(&l3);
        set_tab_override(&mut l3, &id, f, Override::Off);
        assert!(!protection_reduced(&l3), "…nor is one tab opting out of it");

        // The master switch still counts, exactly as before.
        let mut master = settings();
        master.set_master_for_test(false);
        assert!(protection_reduced(&master));
    }

    /// (d) The cross-module invariant, backend half: every report row's
    /// `master_gated` is [`Feature::master_gated`]'s answer, so the frontend
    /// twin can filter on it instead of restating the rule in TypeScript.
    #[test]
    fn every_report_row_publishes_its_master_gating() {
        let s = settings();
        let id = a_tab(&s);
        for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::AppWide] {
            for (row, f) in report(&s, scope).iter().zip(Feature::ALL) {
                assert_eq!(row.master_gated, f.master_gated(), "{}", f.key());
            }
        }
        assert_eq!(
            report(&s, Scope::AppWide)
                .iter()
                .filter(|r| !r.master_gated)
                .map(|r| r.feature)
                .collect::<Vec<_>>(),
            vec!["tool_steering"]
        );
    }

    /// The worker pseudo-scope carries its own row, independent of every tab's.
    #[test]
    fn the_worker_scope_is_independent_of_every_tab() {
        let mut s = settings();
        let id = a_tab(&s);
        s.set_worker_override_for_test(Feature::Canary, Override::Off)
            .expect("the canary is a worker-scoped feature");
        assert!(!effective(Feature::Canary, Scope::OffloadWorker, &s));
        // The canary has no tab row at all, so a tab still reports the L2
        // answer for it (and the report marks it out of scope).
        assert!(effective(Feature::Canary, tab_scope(&id), &s));
        let rows = report(&s, tab_scope(&id));
        let canary = rows.iter().find(|r| r.feature == "canary").unwrap();
        assert!(!canary.in_scope);
    }

    /// Structural claim: the per-scope override structs carry exactly the
    /// features their scope declares. `set` returning `None` is what makes the
    /// "no row here" case impossible to write by accident.
    #[test]
    fn override_rows_match_the_declared_scopes() {
        for f in Feature::ALL {
            let mut tab = TabInjectionOverrides::default();
            assert_eq!(
                tab.set(*f, Override::Off).is_some(),
                f.has_tab_scope(),
                "{f:?} tab row"
            );
            let mut worker = WorkerInjectionOverrides::default();
            assert_eq!(
                worker.set(*f, Override::Off).is_some(),
                f.has_worker_scope(),
                "{f:?} worker row"
            );
            // A feature with no row anywhere is the app-wide one, and there is
            // exactly one of those.
            if !f.has_tab_scope() && !f.has_worker_scope() {
                assert_eq!(*f, Feature::TerminalEscapeHygiene);
            }
        }
        assert_eq!(
            Feature::ALL
                .iter()
                .filter(|f| !f.has_tab_scope() && !f.has_worker_scope())
                .count(),
            1
        );
        // Worker-only: exactly the canary.
        let worker_only: Vec<_> = Feature::ALL
            .iter()
            .filter(|f| f.has_worker_scope() && !f.has_tab_scope())
            .collect();
        assert_eq!(worker_only, vec![&Feature::Canary]);
    }

    /// Native-web reconciliation: the tri-mode IS the L2, and an L3 `On` over an
    /// app-wide `off` re-enables at the default posture.
    #[test]
    fn native_web_mode_composes_with_the_hierarchy() {
        let mut s = settings();
        let id = a_tab(&s);
        // Default: sensor everywhere.
        assert_eq!(native_web_mode(&s, tab_scope(&id)), NativeWebMode::Sensor);
        assert!(effective(Feature::NativeWeb, tab_scope(&id), &s));

        // The mode's own `off` disables the feature — no second switch.
        s.offload.native_web_visibility = "off".into();
        assert!(!effective(Feature::NativeWeb, tab_scope(&id), &s));
        assert_eq!(native_web_mode(&s, tab_scope(&id)), NativeWebMode::Off);

        // L3 On over an app-wide off ⇒ the default posture, `sensor`.
        set_tab_override(&mut s, &id, Feature::NativeWeb, Override::On);
        assert_eq!(native_web_mode(&s, tab_scope(&id)), NativeWebMode::Sensor);

        // `deny` survives the resolution unchanged when the feature is on…
        s.offload.native_web_visibility = "deny".into();
        set_tab_override(&mut s, &id, Feature::NativeWeb, Override::Inherit);
        assert_eq!(native_web_mode(&s, tab_scope(&id)), NativeWebMode::Deny);
        // …and an L3 Off closes it back to pre-V32 behaviour for that tab only.
        set_tab_override(&mut s, &id, Feature::NativeWeb, Override::Off);
        assert_eq!(native_web_mode(&s, tab_scope(&id)), NativeWebMode::Off);
        assert_eq!(native_web_mode(&s, tab_scope("other")), NativeWebMode::Deny);

        // The master switch still wins.
        s.offload.injection.protection = false;
        assert_eq!(native_web_mode(&s, tab_scope("other")), NativeWebMode::Off);
        // An unrecognized mode reads as `sensor` (post-hoc validation).
        assert_eq!(NativeWebMode::parse("SENSOR"), NativeWebMode::Sensor);
        assert_eq!(NativeWebMode::parse(""), NativeWebMode::Sensor);
    }

    /// Budgets and detection resolve through the same hierarchy — the two
    /// helpers exist precisely so their call sites do not read raw fields.
    #[test]
    fn budget_and_detection_helpers_follow_the_hierarchy() {
        let mut s = settings();
        let limits = budget_limits(&s, Scope::OffloadWorker);
        assert_eq!(limits.max_calls, s.offload.external_fetch_max_calls);
        assert_eq!(limits.max_bytes, s.offload.external_fetch_max_bytes);
        s.offload.injection.fetch_budgets_enabled = false;
        let off = budget_limits(&s, Scope::OffloadWorker);
        assert_eq!((off.max_calls, off.max_bytes), (0, 0));

        let cfg = detection_config(&s, Scope::AppWide);
        assert!(cfg.signature && cfg.classifier);
        s.offload.injection.detection_enabled = false;
        let cfg = detection_config(&s, Scope::AppWide);
        assert!(!cfg.signature && !cfg.classifier);
        // The parent wins over the sub-toggles, in both directions: with the
        // parent off, turning a layer on changes nothing.
        s.offload.detection_signature_enabled = true;
        assert!(!detection_config(&s, Scope::AppWide).signature);
    }

    /// One Claude tab and one OpenCode tab — the shape the per-consumer spawn
    /// signature has to tell apart (#48, F-x).
    fn settings_both_consumers() -> Settings {
        let mut opencode = super::super::schema::default_opencode_tab();
        if let TabConfig::AiTool(c) = &mut opencode {
            c.injection_overrides = TabInjectionOverrides::default();
        }
        // All-`Inherit` rows, for the reason [`settings`] gives: these tests
        // move ONE level at a time and read the signature back, which a fixture
        // that already states every cell at L3 would flatten.
        Settings {
            tabs: vec![inheriting_tab(), opencode],
            ..Settings::default()
        }
    }

    /// The id of the first tab whose command launches `consumer`.
    fn tab_of(s: &Settings, consumer: Consumer) -> String {
        s.tabs
            .iter()
            .find_map(|t| match t {
                TabConfig::AiTool(c) if Consumer::for_command(&c.command) == Some(consumer) => {
                    Some(c.id.clone())
                }
                _ => None,
            })
            .expect("a tab for this consumer")
    }

    /// The spawn signature moves for spawn-baked features at BOTH levels, stays
    /// put for the live ones — and, since #48 (F-x), moves for the RIGHT
    /// CONSUMER.
    ///
    /// Both consumer objects used to embed the identical blob, so an
    /// OpenCode-only flip (the Phase H gate; a native-web override on the
    /// OpenCode tab) marked Claude tabs dirty and nagged them to restart for a
    /// change that cannot reach them — the feature that introduced the rule
    /// "a restart nag for a change that needs no restart is how a hint stops
    /// being read" violating it. The third column below is the property: exactly
    /// which consumers each flip is allowed to disturb.
    ///
    /// #48 (M-3) moved `spotlighting` out of the `stays` list and into `moves`:
    /// its `stays` entry PINNED THE DEFECT, because `fact_promotion_block`
    /// resolves the control at LAUNCH and bakes envelope-or-not into the system
    /// prompt. It is the one feature in both columns — live at the proxy, baked
    /// in the addendum — and `spawn_baked` answers "does the user owe this a
    /// restart?", so it belongs here.
    #[test]
    fn the_spawn_signature_tracks_only_spawn_baked_levels() {
        use Consumer::{Claude, Opencode};
        let base = settings_both_consumers();
        let before = |c: Consumer| spawn_sig(&base, c);

        type Mutate = Box<dyn Fn(&mut Settings)>;
        let moves: Vec<(&str, Mutate, &[Consumer])> = vec![
            (
                "native-web L2 (the mode)",
                Box::new(|s: &mut Settings| s.set_native_web_mode_for_test(NativeWebMode::Deny)),
                &[Claude, Opencode],
            ),
            (
                "native-web L3 on the Claude tab",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Claude);
                    set_tab_override(s, &id, Feature::NativeWeb, Override::Off);
                }),
                &[Claude],
            ),
            (
                "native-web L3 on the OpenCode tab",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Opencode);
                    set_tab_override(s, &id, Feature::NativeWeb, Override::Off);
                }),
                &[Opencode],
            ),
            (
                "consumer-hygiene L2",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::ConsumerHygiene, false)),
                &[Claude, Opencode],
            ),
            (
                "consumer-hygiene L3 on the Claude tab",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Claude);
                    set_tab_override(s, &id, Feature::ConsumerHygiene, Override::Off);
                }),
                &[Claude],
            ),
            // The managed-tool steering paragraph rides the same channel as the
            // hygiene one, for both consumers.
            (
                "tool-steering L2",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::ToolSteering, false)),
                &[Claude, Opencode],
            ),
            (
                "tool-steering L3 on the Claude tab",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Claude);
                    set_tab_override(s, &id, Feature::ToolSteering, Override::Off);
                }),
                &[Claude],
            ),
            // V32 Phase H: both levels of the OpenCode native gate. Its flag is
            // compiled into the generated plugin, so a flip that does not move
            // the OpenCode signature is a gate the user believes is on and is
            // not — and one that moves the CLAUDE signature is the F-x nag.
            (
                "opencode-native-gate L2",
                // `false`, not `true`: V39 ships this L2 ON like every other, so
                // writing `true` here would assert that a no-op flip moves a
                // signature. The property under test is unchanged — one level of
                // one control, and exactly which consumers may notice.
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::OpencodeNativeGate, false)),
                &[Opencode],
            ),
            (
                "opencode-native-gate L3",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Opencode);
                    set_tab_override(s, &id, Feature::OpencodeNativeGate, Override::Off);
                }),
                &[Opencode],
            ),
            // #48 (M-3): spotlighting is BOTH live and spawn-baked. The baked
            // half is `fact_promotion_block`'s decision to envelope the pinned-
            // memory addendum, taken at launch and written into the system
            // prompt — so a mid-session flip owes a restart hint. This entry
            // used to live in the `stays` list below and PINNED THE DEFECT.
            (
                "spotlighting L2 (the memory addendum is baked)",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::Spotlighting, false)),
                &[Claude, Opencode],
            ),
            (
                "spotlighting L3 on the Claude tab",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Claude);
                    set_tab_override(s, &id, Feature::Spotlighting, Override::Off);
                }),
                &[Claude],
            ),
            (
                // L1 reaches every launch there is, so both objects carry it.
                "the global master",
                Box::new(|s: &mut Settings| s.set_master_for_test(false)),
                &[Claude, Opencode],
            ),
        ];
        for (name, mutate, expect) in moves {
            let mut s = settings_both_consumers();
            mutate(&mut s);
            for c in [Claude, Opencode] {
                let moved = spawn_sig(&s, c) != before(c);
                assert_eq!(
                    moved,
                    expect.contains(&c),
                    "{name}: {c:?} signature moved={moved}, expected={}",
                    expect.contains(&c)
                );
            }
        }

        // Live features must NOT move it — a restart hint for a change that
        // takes effect on the next call is how a hint stops being read.
        let stays: Vec<(&str, Mutate)> = vec![
            (
                "taint latch L2",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::TaintLatch, false)),
            ),
            (
                "detection L2",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::Detection, false)),
            ),
            (
                "fetch budgets L2",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::FetchBudgets, false)),
            ),
        ];
        for (name, mutate) in stays {
            let mut s = settings_both_consumers();
            mutate(&mut s);
            for c in [Claude, Opencode] {
                assert_eq!(
                    spawn_sig(&s, c),
                    before(c),
                    "{name} must not move the {c:?} spawn signature"
                );
            }
        }
    }

    /// [`Feature::ToolSteering`]'s second input, and the thing that must stay
    /// OUT of the signature.
    ///
    /// The paragraph's `run_command` half is written only when this consumer's
    /// `tool_plugins.expose_commands_*` flag is on at spawn, so that flag is a
    /// spawn-baked input like the native-web mode — and per-consumer, so
    /// flipping Claude's must not nag OpenCode tabs.
    ///
    /// The other half is the one that would make the paragraph unusable: a
    /// Detect, an enable/disable or a path change in the TOOL REGISTRY must NOT
    /// move it. Those inputs are LIVE — `graph::mcp`'s native pulse
    /// re-advertises the `run_check` `name` / `run_command` `tool` enums to a
    /// tab that is already running — and the paragraph deliberately names none
    /// of them, so a restart hint for a registry edit would fire constantly and
    /// mean nothing.
    #[test]
    fn tool_steering_rides_the_expose_flag_but_not_the_tool_registry() {
        use Consumer::{Claude, Opencode};
        let base = settings_both_consumers();
        let claude_before = spawn_sig(&base, Claude);
        let opencode_before = spawn_sig(&base, Opencode);

        let mut s = settings_both_consumers();
        s.tool_plugins.expose_commands_claude = false;
        assert_ne!(
            spawn_sig(&s, Claude),
            claude_before,
            "hiding `run_command` from Claude changes the paragraph a fresh Claude tab is \
             launched with"
        );
        assert_eq!(
            spawn_sig(&s, Opencode),
            opencode_before,
            "…and cannot reach an OpenCode tab, so it must not nag one"
        );

        let mut s = settings_both_consumers();
        s.tool_plugins.expose_commands_opencode = false;
        assert_ne!(spawn_sig(&s, Opencode), opencode_before);
        assert_eq!(spawn_sig(&s, Claude), claude_before);

        // With the feature resolved off for every tab, the flag cannot change
        // what any fresh tab writes — and a nag for a change with no effect is
        // how a restart hint stops being read.
        let mut off = settings_both_consumers();
        off.set_l2_for_test(Feature::ToolSteering, false);
        let claude_off = spawn_sig(&off, Claude);
        let opencode_off = spawn_sig(&off, Opencode);
        off.tool_plugins.expose_commands_claude = false;
        off.tool_plugins.expose_commands_opencode = false;
        assert_eq!(spawn_sig(&off, Claude), claude_off, "steering off ⇒ the flag is inert");
        assert_eq!(spawn_sig(&off, Opencode), opencode_off);

        // The registry itself: none of these may move either object.
        type Mutate = Box<dyn Fn(&mut Settings)>;
        let registry_edits: Vec<(&str, Mutate)> = vec![
            (
                "a machine-wide path (what Detect writes)",
                Box::new(|s: &mut Settings| {
                    s.tool_plugins
                        .global_paths
                        .insert("demo@1/rg".to_string(), "C:\\bin\\rg.exe".to_string());
                }),
            ),
            (
                "a per-project path override",
                Box::new(|s: &mut Settings| {
                    s.tool_plugins.project_paths.insert(
                        "P:\\proj".to_string(),
                        [("demo@1/rg".to_string(), "D:\\rg.exe".to_string())]
                            .into_iter()
                            .collect(),
                    );
                }),
            ),
            (
                "disabling a plugin",
                Box::new(|s: &mut Settings| {
                    s.tool_plugins.plugins.insert(
                        "demo@1".to_string(),
                        crate::settings::schema::PluginState {
                            enabled: false,
                            tools: Default::default(),
                        },
                    );
                }),
            ),
            (
                "disabling one tool inside a plugin",
                Box::new(|s: &mut Settings| {
                    s.tool_plugins.plugins.insert(
                        "demo@1".to_string(),
                        crate::settings::schema::PluginState {
                            enabled: true,
                            tools: [(
                                "rg".to_string(),
                                crate::settings::schema::ToolState {
                                    enabled: false,
                                    ..Default::default()
                                },
                            )]
                            .into_iter()
                            .collect(),
                        },
                    );
                }),
            ),
        ];
        for (name, mutate) in registry_edits {
            let mut s = settings_both_consumers();
            mutate(&mut s);
            assert_eq!(
                spawn_sig(&s, Claude),
                claude_before,
                "{name} is a LIVE surface change — it must not move the Claude spawn signature"
            );
            assert_eq!(
                spawn_sig(&s, Opencode),
                opencode_before,
                "{name} is a LIVE surface change — it must not move the OpenCode spawn signature"
            );
        }
    }

    /// #48 (M-3): the launch-time reader and the restart-hint list must agree.
    ///
    /// The real guard is the const assert at
    /// `tabs::config::fact_promotion_block`'s `SPOTLIGHT_AT_SPAWN` — this test
    /// exists so the FAILURE is legible: a build error there says "const eval
    /// failed", and this says what the invariant was.
    #[test]
    fn every_control_baked_into_a_launch_is_declared_spawn_baked() {
        assert!(
            Feature::Spotlighting.spawn_baked(),
            "`fact_promotion_block` resolves spotlighting at LAUNCH and writes the answer into \
             the system prompt; without this the user gets no restart hint and a tab toggled ON \
             mid-session keeps injecting UNENVELOPED pre-V32 memory"
        );
        // …and the predicate is const, which is what makes the call site a
        // build error rather than a comment.
        const _: Feature = Feature::Spotlighting.baked_at_spawn();
    }

    /// **Splitting the signature must not DROP a cell** (#48, F-x). Every AI tab
    /// appears in exactly one consumer's rows, and every spawn-baked feature is
    /// carried by at least one consumer — so the union of the two objects covers
    /// what the single shared blob covered, which is the contract
    /// `spawn_inject_sig` has in this repo.
    #[test]
    fn the_per_consumer_split_partitions_the_tabs_and_covers_every_feature() {
        let s = settings_both_consumers();
        let ids = |c: Consumer| -> Vec<String> {
            spawn_sig(&s, c)["tabs"]
                .as_array()
                .expect("tabs array")
                .iter()
                .map(|row| row[0].as_str().expect("tab id").to_string())
                .collect()
        };
        let mut union = ids(Consumer::Claude);
        union.extend(ids(Consumer::Opencode));
        union.sort();
        let mut all: Vec<String> = s
            .tabs
            .iter()
            .filter_map(|t| match t {
                TabConfig::AiTool(c) => Some(c.id.clone()),
                _ => None,
            })
            .collect();
        all.sort();
        assert_eq!(union, all, "every AI tab belongs to exactly one consumer");

        // …and every spawn-baked feature is read by at least one of them.
        for f in Feature::ALL.iter().filter(|f| f.spawn_baked()) {
            assert!(
                Consumer::Claude.reads(*f) || Consumer::Opencode.reads(*f),
                "{} is spawn-baked but reaches no consumer, so no hint would ever fire",
                f.key()
            );
        }
    }

    /// The reduced-protection predicate — the input to the out-of-Settings
    /// indicator — fires for the master switch, an app-wide feature, the worker
    /// row, and an app-wide flag a TAB is inheriting.
    ///
    /// **A tab's own `Off` cell no longer counts** (V39): a new tab ships every
    /// tab-scoped cell off, so counting them would raise the chip on every tab
    /// of every fresh install. The last two cases below are the narrowing and
    /// the hole it deliberately leaves closed — see [`protection_reduced`].
    #[test]
    fn protection_reduced_sees_every_level() {
        assert!(!protection_reduced(&settings()));
        let mut s = settings();
        s.offload.injection.protection = false;
        assert!(protection_reduced(&s));

        let mut s = settings();
        s.offload.injection.terminal_escape_hygiene_enabled = false;
        assert!(protection_reduced(&s), "an app-wide feature counts");

        let mut s = settings();
        s.set_worker_override_for_test(Feature::Canary, Override::Off)
            .expect("the canary is a worker-scoped feature");
        assert!(protection_reduced(&s), "the worker scope counts");

        let mut s = settings();
        let id = a_tab(&s);
        set_tab_override(&mut s, &id, Feature::SsrfGuard, Override::Off);
        assert!(
            !protection_reduced(&s),
            "a tab's own cell is the tab BASELINE since V39, not a reduction"
        );
        // …and a fresh install, every cell of which is exactly that, agrees.
        assert!(!protection_reduced(&fresh_install()));

        // THE HOLE THAT MUST STAY CLOSED. Consumer hygiene, memory quarantine
        // and native-web visibility carry a tab row and no other row, so if the
        // tab pass were dropped outright rather than narrowed, switching one of
        // them off APP-WIDE would be invisible to every surface — while every
        // inheriting tab really did lose it.
        for f in Feature::ALL
            .iter()
            .filter(|f| f.master_gated() && f.has_tab_scope() && !f.has_worker_scope())
        {
            let mut s = settings();
            s.set_l2_for_test(*f, false);
            assert!(
                protection_reduced(&s),
                "{f:?} switched off app-wide must still count — no other scope has a row for it"
            );
        }
        // The same flip on a fresh install is NOT reduced, and that is correct
        // rather than a gap: those tabs state `off` themselves, so the app-wide
        // flip took nothing away from them. Tab-ONLY controls, because a feature
        // that also has a worker row is still lost there — the worker's cells
        // are untouched by the per-tab baseline and its pass still counts them.
        for f in Feature::ALL
            .iter()
            .filter(|f| f.has_tab_scope() && !f.has_worker_scope())
        {
            let mut s = fresh_install();
            s.set_l2_for_test(*f, false);
            assert!(!protection_reduced(&s), "{f:?}");
        }
    }

    /// Keys are wire values: stable, unique, and matching the enum's serde form
    /// so a Settings write can name a feature by the key it was rendered with.
    #[test]
    fn feature_keys_are_unique_and_round_trip() {
        let mut seen = Vec::new();
        for f in Feature::ALL {
            assert!(!seen.contains(&f.key()), "duplicate key {}", f.key());
            seen.push(f.key());
            assert!(!f.label().is_empty());
            let json = serde_json::to_string(f).unwrap();
            assert_eq!(json, format!("\"{}\"", f.key()));
            assert_eq!(serde_json::from_str::<Feature>(&json).unwrap(), *f);
        }
        // No `seen.len() == Feature::ALL.len()` line here any more: it was
        // tautological (the loop pushes once per entry, and the uniqueness
        // assert above is what actually rules out a repeat), and the thing it
        // *looked* like it checked — that `ALL` covers the enum — is now
        // guaranteed by construction, since `declare_features!` builds the
        // array from the variant list (#47).
    }

    /// Overrides tolerate a hand-edited settings file: unknown ⇒ `Inherit`, and
    /// the wire form round-trips.
    #[test]
    fn override_parsing_is_post_hoc_and_neutral_on_junk() {
        assert_eq!(Override::parse("on"), Override::On);
        assert_eq!(Override::parse(" off "), Override::Off);
        // Everything except the two recognized words reads as Inherit.
        for junk in ["", "ON", "true", "yes", "inherit", "nonsense"] {
            assert_eq!(Override::parse(junk), Override::Inherit, "{junk}");
        }
        let row = TabInjectionOverrides {
            taint_latch: Override::Off,
            ..Default::default()
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"taint_latch\":\"off\""), "{json}");
        assert_eq!(
            serde_json::from_str::<TabInjectionOverrides>(&json).unwrap(),
            row
        );
        // A junk cell deserializes to Inherit rather than failing the whole
        // settings load.
        let junk: TabInjectionOverrides =
            serde_json::from_str(r#"{"taint_latch":"maybe"}"#).unwrap();
        assert_eq!(junk.taint_latch, Override::Inherit);
    }

    /// **#48, G-1 — the shapes the test above did not feed it.**
    ///
    /// The guard test up there passes only STRINGS, which is why it stayed green
    /// while `#[serde(from = "String")]` rejected everything else: the post-hoc
    /// parse never ran for a value that failed to type as a string, and
    /// `#[serde(default)]` fires for an absent key, not for a present one that
    /// will not deserialize. `{"taint_latch": true}` — the intuitive typo, since
    /// the control it overrides IS a boolean — and `{"taint_latch": null}` — the
    /// intuitive way to clear a cell — therefore failed the typed parse of the
    /// whole settings file and reset every setting in it.
    ///
    /// So this enumerates the JSON type space, not a list of plausible typos.
    #[test]
    fn a_non_string_override_cell_reads_as_inherit() {
        for junk in [
            r#"{"taint_latch":true}"#,
            r#"{"taint_latch":false}"#,
            r#"{"taint_latch":null}"#,
            r#"{"taint_latch":1}"#,
            r#"{"taint_latch":0}"#,
            r#"{"taint_latch":-1}"#,
            r#"{"taint_latch":0.5}"#,
            r#"{"taint_latch":[]}"#,
            r#"{"taint_latch":["on"]}"#,
            r#"{"taint_latch":{}}"#,
            r#"{"taint_latch":{"value":"on"}}"#,
        ] {
            let row: TabInjectionOverrides = serde_json::from_str(junk)
                .unwrap_or_else(|e| panic!("{junk} must not fail the parse: {e}"));
            assert_eq!(row.taint_latch, Override::Inherit, "{junk}");
            // …and no neighbouring cell moved either.
            assert_eq!(row, TabInjectionOverrides::default(), "{junk}");
        }
        // The worker row is the same type, so it inherits the property; asserted
        // rather than assumed, since it is a separate struct with its own
        // `#[serde(default)]`.
        for junk in [r#"{"canary":true}"#, r#"{"canary":null}"#] {
            let row: WorkerInjectionOverrides = serde_json::from_str(junk).expect(junk);
            assert_eq!(row.canary, Override::Inherit, "{junk}");
        }
    }

    /// **The contract G-1 states, at the level it is stated at**: "a hand-edited
    /// typo must neither grant protection nor remove it, and must not quarantine
    /// the settings file."
    ///
    /// The unit test above covers the two rows; this drives a whole `Settings`
    /// round trip — the shape `settings::persistence` actually deserializes,
    /// where a failure means `quarantine_corrupt_file` and seeded defaults for
    /// themes, tabs, backends, checks, MCP servers and pricing.
    #[test]
    fn a_non_string_override_cell_neither_quarantines_the_file_nor_moves_protection() {
        for bogus in [
            serde_json::json!(true),
            serde_json::json!(false),
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!("maybe"),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let s = settings();
            let id = a_tab(&s);
            let mut v = serde_json::to_value(&s).expect("settings serialize");
            for t in v["tabs"].as_array_mut().expect("tabs is an array") {
                if t.get("id").and_then(|x| x.as_str()) == Some(id.as_str()) {
                    t["injection_overrides"]["taint_latch"] = bogus.clone();
                }
            }
            v["offload"]["injection"]["worker"]["canary"] = bogus.clone();

            let back: Settings = serde_json::from_value(v)
                .unwrap_or_else(|e| panic!("{bogus} quarantines the settings file: {e}"));
            // Neither granted nor removed: both cells resolve exactly as an
            // untouched file does.
            assert_eq!(
                effective(Feature::TaintLatch, tab_scope(&id), &back),
                Feature::TaintLatch.default_enabled(),
                "{bogus}"
            );
            assert_eq!(
                effective(Feature::Canary, Scope::OffloadWorker, &back),
                Feature::Canary.default_enabled(),
                "{bogus}"
            );
            // …and the rest of the file survived, which is the whole finding.
            assert_eq!(back.tabs.len(), s.tabs.len(), "{bogus}");
        }
    }

    /// **#48, N-1 — an identity-less call must not silently ignore a per-tab
    /// L3 `On`.**
    ///
    /// [`Scope::for_tab`] maps a missing `--tab` to [`Scope::UnknownCaller`]
    /// (which was [`Scope::AppWide`]'s other half until F-35 split them),
    /// documented as
    /// unconditionally fail-OPEN. That reading holds only while L2 ≥ L3, and
    /// locked decision 17 ships the configuration that inverts it: one hardened
    /// tab (L3 `On`) over an app-wide `Off`. The app-wide answer is `off`, so a
    /// call from that tab — a user-configured MCP entry invoking
    /// `cimp --offload-mcp` without `--tab`, or a pre-V28 generated
    /// `--mcp-config` — ran unprotected while Settings showed it as on.
    #[test]
    fn an_identity_less_call_honours_any_tabs_scope_on() {
        let mut s = settings();
        let id = a_tab(&s);
        // Decision 17's shape: an L2 `off` with one tab hardened over it. Since
        // V39 this L2 ships ON like every other, so the test states the `off`
        // itself rather than borrowing a default that has moved.
        let f = Feature::OpencodeNativeGate;
        s.set_l2_for_test(f, false);

        // Nothing overridden: the app-wide answer is the L2 value, decided at
        // L2. The elevation must not fire on a config nobody overrode.
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: false,
                decided_by: DecidedBy::Feature
            }
        );

        // One hardened tab. The identity-less call now resolves ON.
        set_tab_override(&mut s, &id, f, Override::On);
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: true,
                decided_by: DecidedBy::Scope
            },
            "a call with no identity may be from the hardened tab"
        );
        // The known scopes are untouched: the tab that asked for it, and one
        // that did not.
        assert!(effective(f, tab_scope(&id), &s));
        assert!(!effective(f, tab_scope("some-other-tab"), &s));

        // Same shape over an L2 that is ON for a normally-on feature: an L3
        // `Off` must NOT travel up, or the elevation would be a downgrade path.
        let mut s = settings();
        set_tab_override(&mut s, &id, Feature::TaintLatch, Override::Off);
        assert_eq!(
            decide(Feature::TaintLatch, Scope::UnknownCaller, &s),
            Decision {
                effective: true,
                decided_by: DecidedBy::Feature
            },
            "only `On` travels up — this can add protection, never remove it"
        );

        // L1 still short-circuits everything, elevation included.
        let mut s = settings();
        set_tab_override(&mut s, &id, f, Override::On);
        s.set_master_for_test(false);
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: false,
                decided_by: DecidedBy::Global
            }
        );

        // The worker keeps its own answer: it always has an identity, so the
        // elevation must not reach it.
        let mut s = settings();
        set_tab_override(&mut s, &id, Feature::Canary, Override::On);
        assert_eq!(
            decide(Feature::Canary, Scope::OffloadWorker, &s).decided_by,
            DecidedBy::Feature,
            "the canary has no tab cell to elevate from, and the worker has its own row"
        );

        // Structural: the elevation cannot flip an APP-ONLY control, because a
        // feature with no tab row can never carry a tab `On`. That is what keeps
        // `protection_reduced`'s app-scope pass — which only ever inspects
        // app-only controls — reading exactly as before.
        for feature in Feature::ALL.iter().filter(|f| !f.has_tab_scope()) {
            let mut s = settings();
            s.set_l2_for_test(*feature, false);
            assert!(
                !effective(*feature, Scope::UnknownCaller, &s),
                "{feature:?} has no tab row, so nothing can elevate it"
            );
        }
        // …stated at the level that matters: an app-only control switched off is
        // still reduced protection.
        let mut s = settings();
        set_tab_override(&mut s, &id, Feature::TaintLatch, Override::On);
        s.set_l2_for_test(Feature::TerminalEscapeHygiene, false);
        assert!(protection_reduced(&s));
    }

    /// **N-1's exact width, pinned before `Scope::App` was split (F-35,
    /// locked decision 36).**
    ///
    /// The split introduces [`armed_anywhere`], which DOES fold the offload
    /// worker's row in, a few lines from the elevation that must not. The two
    /// invariants this locks:
    ///
    /// - **only `On` travels up** — an L3 `Off` stays where the user put it, so
    ///   this can never remove protection, only add it to a caller with no
    ///   identity;
    /// - **it never applies to a scope that HAS an answer** — a known tab, or
    ///   the worker.
    ///
    /// Driven with [`Feature::Detection`] deliberately: it is the only kind of
    /// feature where the tab/worker distinction is observable at all (a tab row
    /// AND a worker row), and it is F-35's own feature.
    /// [`an_identity_less_call_honours_any_tabs_scope_on`] uses
    /// `OpencodeNativeGate` (no worker row) and `Canary` (no tab row), so a
    /// regression that folded the worker into [`any_tab_override_on`] leaves it
    /// green. That is the regression this test exists for.
    ///
    /// It landed against the pre-split code with `Scope::App` at every scope
    /// argument, and the split changed **only that token** to
    /// [`Scope::UnknownCaller`] — diff it that way, because any other edit to it
    /// during the refactor is the refactor telling you it widened or narrowed
    /// N-1.
    #[test]
    fn n1_carries_an_on_up_from_configured_tabs_only() {
        let f = Feature::Detection;
        let id = a_tab(&settings());

        // 1. Untouched: the elevation does not fire on a config nobody
        //    overrode.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: false,
                decided_by: DecidedBy::Feature
            },
            "no override anywhere ⇒ the L2 answer, decided at L2"
        );

        // 2. Only `On` travels up.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        set_tab_override(&mut s, &id, f, Override::On);
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: true,
                decided_by: DecidedBy::Scope
            },
            "a call with no identity may be from the hardened tab"
        );

        // 3. `Off` never travels up — this can add protection, never remove it.
        let mut s = settings(); // L2 on for Detection by default
        set_tab_override(&mut s, &id, f, Override::Off);
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: true,
                decided_by: DecidedBy::Feature
            },
            "an L3 Off stays where the user put it"
        );
        assert!(
            !effective(f, tab_scope(&id), &s),
            "…and stays in force there"
        );

        // 4. TABS ONLY. The worker's row must NOT travel up. This is the
        //    assertion `armed_anywhere` exists to violate deliberately, and
        //    nothing else may.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        s.set_worker_override_for_test(f, Override::On)
            .expect("detection has a worker row");
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: false,
                decided_by: DecidedBy::Feature
            },
            "the worker always has an identity, so it is never the caller behind an \
             identity-less call — folding its row in would raise protection for a \
             population it does not describe"
        );
        assert!(
            effective(f, Scope::OffloadWorker, &s),
            "…and keeps its own On"
        );

        // 5. It never reaches a scope that HAS an answer.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        set_tab_override(&mut s, &id, f, Override::On);
        assert!(effective(f, tab_scope(&id), &s), "the tab that asked for it");
        assert!(
            !effective(f, tab_scope("a-tab-that-did-not"), &s),
            "a known tab keeps L2"
        );
        assert!(
            !effective(f, Scope::OffloadWorker, &s),
            "the worker keeps its own row"
        );

        // 6. Per feature, not per tab: one feature's `On` elevates only that
        //    feature.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        s.set_l2_for_test(Feature::SsrfGuard, false);
        set_tab_override(&mut s, &id, f, Override::On);
        assert!(effective(f, Scope::UnknownCaller, &s));
        assert!(
            !effective(Feature::SsrfGuard, Scope::UnknownCaller, &s),
            "a tab hardening one control does not harden the rest"
        );

        // 7. The MAPPING, not just the variant: a call with no `--tab` — or a
        //    blank one — lands on the elevating scope, and a real one does not.
        assert_eq!(Scope::for_tab("claude", None), Scope::UnknownCaller);
        assert_eq!(Scope::for_tab("claude", Some("   ")), Scope::UnknownCaller);
        assert_eq!(
            Scope::for_tab("claude", Some(&id)),
            Scope::Tab {
                agent: "claude",
                tab: &id
            }
        );

        // 8. L1 short-circuits the elevation like everything else.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        set_tab_override(&mut s, &id, f, Override::On);
        s.set_master_for_test(false);
        assert_eq!(
            decide(f, Scope::UnknownCaller, &s),
            Decision {
                effective: false,
                decided_by: DecidedBy::Global
            }
        );
    }

    /// **#48 F-35 — `armed_anywhere` is the third question, and it is exactly
    /// one row wider than N-1.**
    ///
    /// It sits beside an elevation that must NOT fold the worker in and
    /// deliberately does, so the difference is asserted as a difference: the
    /// same settings, the two predicates, opposite answers. Everything else it
    /// must keep — L1 short-circuiting, `Off` not travelling up, per-feature
    /// scoping — is asserted too, because a predicate this permissive is exactly
    /// the one a future reader will reach for as a gate.
    #[test]
    fn armed_anywhere_is_n1_plus_the_worker_row_and_nothing_else() {
        let f = Feature::Detection;
        let id = a_tab(&settings());

        // Baseline: on app-wide ⇒ armed, trivially.
        let s = settings();
        assert!(armed_anywhere(f, &s));

        // THE DIFFERENCE. Narrowed to the worker: N-1 says no, this says yes,
        // and F-35 is the gap between those two answers.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        s.set_worker_override_for_test(f, Override::On)
            .expect("detection has a worker row");
        assert!(
            !effective(f, Scope::UnknownCaller, &s),
            "the identity-less caller is not the worker — N-1 is unchanged"
        );
        assert!(!effective(f, Scope::AppWide, &s), "and the baseline is off");
        assert!(
            armed_anywhere(f, &s),
            "but the worker IS scanning, which is the whole of F-35"
        );

        // A tab's `On` counts as well, through the N-1 half.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        assert!(!armed_anywhere(f, &s), "nobody is armed");
        set_tab_override(&mut s, &id, f, Override::On);
        assert!(armed_anywhere(f, &s), "that tab is armed");

        // An `Off` still travels nowhere: one tab opting out does not disarm
        // the rest, so this can never read as "nobody is scanning" while
        // somebody is.
        let mut s = settings();
        set_tab_override(&mut s, &id, f, Override::Off);
        s.set_worker_override_for_test(f, Override::Off)
            .expect("detection has a worker row");
        assert!(
            armed_anywhere(f, &s),
            "L2 is still on, so every other tab is scanning"
        );

        // L1 closes it everywhere, like everything else built on `decide`.
        let mut s = settings();
        s.set_worker_override_for_test(f, Override::On)
            .expect("detection has a worker row");
        set_tab_override(&mut s, &id, f, Override::On);
        s.set_master_for_test(false);
        for feature in Feature::ALL.iter().filter(|f| f.master_gated()) {
            assert!(
                !armed_anywhere(*feature, &s),
                "{feature:?} — the master switch always wins"
            );
        }

        // Per feature, not a blanket "anything is on".
        let mut s = settings();
        s.set_l2_for_test(f, false);
        s.set_l2_for_test(Feature::SsrfGuard, false);
        s.set_worker_override_for_test(f, Override::On)
            .expect("detection has a worker row");
        assert!(armed_anywhere(f, &s));
        assert!(
            !armed_anywhere(Feature::SsrfGuard, &s),
            "arming one control does not arm the rest"
        );

        // And the resolved-value form composes the sub-toggle exactly once,
        // like `detection_config` does for a scope.
        let mut s = settings();
        s.set_l2_for_test(f, false);
        s.set_worker_override_for_test(f, Override::On)
            .expect("detection has a worker row");
        assert!(detection_config_anywhere(&s).signature);
        assert!(
            !detection_config(&s, Scope::AppWide).signature,
            "the app-wide reading is still off, and both are available"
        );
        s.set_detection_layer_for_test(DetectionLayer::Signature, false);
        assert!(
            !detection_config_anywhere(&s).signature,
            "the per-layer sub-toggle still wins inside an armed surface"
        );
    }

    /// The report is the introspection contract: one row per feature, naming
    /// the deciding level for each.
    #[test]
    fn the_report_names_the_deciding_level_for_every_combination() {
        let mut s = settings();
        let id = a_tab(&s);
        s.offload.injection.detection_enabled = false;
        set_tab_override(&mut s, &id, Feature::SsrfGuard, Override::Off);
        set_tab_override(&mut s, &id, Feature::TaintLatch, Override::On);
        let rows = report(&s, tab_scope(&id));
        assert_eq!(rows.len(), Feature::ALL.len());
        // #48 F-y: the report carries every property the Settings matrix used to
        // hand-mirror — key, label, scope membership, shipping default and, as
        // of that finding, whether the control is spawn-baked. The matrix
        // renders from these, so a control that stops publishing one loses its
        // row rather than growing a stale copy of it.
        for (row, f) in rows.iter().zip(Feature::ALL) {
            assert_eq!(row.feature, f.key());
            assert_eq!(row.label, f.label());
            assert_eq!(row.default_on, f.default_enabled());
            assert_eq!(row.spawn_baked, f.spawn_baked(), "{}", f.key());
            assert_eq!(row.master_gated, f.master_gated(), "{}", f.key());
        }
        let by = |k: &str| rows.iter().find(|r| r.feature == k).unwrap();
        assert_eq!(by("detection").decided_by, DecidedBy::Feature);
        assert!(!by("detection").effective);
        assert_eq!(by("ssrf_guard").decided_by, DecidedBy::Scope);
        assert_eq!(by("ssrf_guard").override_value, "off");
        assert_eq!(by("taint_latch").decided_by, DecidedBy::Scope);
        assert!(by("taint_latch").effective);
        assert_eq!(by("spotlighting").decided_by, DecidedBy::Feature);

        s.offload.injection.protection = false;
        for r in report(&s, tab_scope(&id)) {
            if !r.master_gated {
                // The master switch is not above this row, so it cannot be the
                // level that decided it — L2/L3 still is, and it is still ON.
                assert_eq!(r.feature, "tool_steering");
                assert_eq!(r.decided_by, DecidedBy::Feature);
                assert!(r.effective);
                continue;
            }
            assert_eq!(r.decided_by, DecidedBy::Global, "{}", r.feature);
            assert!(!r.effective);
        }
    }
}

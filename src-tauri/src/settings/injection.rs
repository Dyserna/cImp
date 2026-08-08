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
//!   `true`). Off disables every V32 control everywhere: all tabs AND the
//!   offload worker. It is the one switch nothing overrides upward.
//! - **L2 — per feature, app-wide.** One `<feature>_enabled` flag per
//!   [`Feature`] (default `true`), with two deliberate exceptions:
//!   [`Feature::NativeWeb`], whose L2 is a tri-mode rather than a boolean (see
//!   *Native-web reconciliation* below), and [`Feature::OpencodeNativeGate`],
//!   whose default is **`false`** (V32 Phase H, locked decision 17 — see
//!   [`Feature::default_enabled`], and note what that means for
//!   [`protection_reduced`]).
//! - **L3 — per scope.** A tri-state [`Override`] (`Inherit` | `On` | `Off`,
//!   default `Inherit`) stored per scope, per feature.
//!
//! # The locked resolution rule
//!
//! ```text
//! if !L1 { false } else { match L3 { On => true, Off => false, Inherit => L2 } }
//! ```
//!
//! An L3 `On` CAN re-enable a feature its L2 default disabled — that is what an
//! override means. NOTHING re-enables past an L1 `off` — that is what a master
//! switch means. [`decide`] is the single implementation; [`effective`] is the
//! boolean-only shorthand.
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
//! only there ([`Feature::Canary`]) carry only that row. [`Scope::App`] is the
//! third: the scope of a control with no per-scope row at all
//! ([`Feature::TerminalEscapeHygiene`] — TTS and toasts are global surfaces per
//! the global-only avatar/TTS decision), and the honest answer for a call that
//! could not resolve a narrower one.
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
//! [`Feature::spawn_baked`] names the features applied when a tab launches
//! ([`Feature::NativeWeb`], [`Feature::ConsumerHygiene`], and Phase H's
//! [`Feature::OpencodeNativeGate`], whose flag is compiled into the generated
//! OpenCode plugin). Their L2 *and* L3 values ride
//! `tabs::config::spawn_inject_sig` via [`spawn_sig`], so flipping any of them
//! raises the restart hint. Every other feature resolves per call and takes
//! effect immediately.

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
    /// V32 Phase H (locked decision 17): the OpenCode plugin's
    /// `tool.execute.before` handler *denying* the harness's own native tools
    /// against the tab's taint latch, rather than only beaconing on them.
    ///
    /// **The one feature whose L2 default is `false`** — see
    /// [`Feature::default_enabled`]. Spawn-baked (the flag is compiled into the
    /// generated plugin) and tab-scoped only: it is delivered by an OpenCode
    /// plugin, so the offload worker has no row for it and neither does any
    /// non-OpenCode consumer.
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
            Feature::OpencodeNativeGate => "OpenCode native-tool gating",
            Feature::TerminalEscapeHygiene => "Terminal escape hygiene",
        }
    }

    /// This feature's **L2 default** — the value an untouched settings file
    /// resolves it to.
    ///
    /// Every V32 control before Phase H defaulted `true`, and one predicate read
    /// that as a law: "any feature resolving off means protection is REDUCED"
    /// ([`protection_reduced`], and its frontend twin `reducedFeaturesFor`).
    /// [`Feature::OpencodeNativeGate`] breaks it — locked decision 17 ships it
    /// **default off**, because whole-surface denial of `bash`/`read`/`edit`
    /// materially changes everyday tab UX and is an opt-in posture, not a
    /// baseline. Without this predicate a fresh install would raise the
    /// reduced-protection chip on every tab out of the box, which is how an
    /// indicator stops being read.
    ///
    /// So "reduced" is measured against the DEFAULT, not against `true`: a
    /// default-off control that is off is the baseline, and one switched on is
    /// *more* protection, never less.
    ///
    /// An exhaustive `match` rather than a `matches!` (#47): a new control's
    /// shipping default is a decision, and falling through to `true` would let
    /// it be taken by omission.
    pub fn default_enabled(self) -> bool {
        match self {
            Feature::OpencodeNativeGate => false,
            Feature::TaintLatch
            | Feature::Spotlighting
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary
            | Feature::MemoryQuarantine
            | Feature::NativeWeb
            | Feature::ConsumerHygiene
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
            | Feature::OpencodeNativeGate
            | Feature::TerminalEscapeHygiene => false,
        }
    }

    /// Whether this feature is applied at TAB SPAWN rather than per call.
    ///
    /// The consumer of this predicate is `tabs::config::spawn_inject_sig` (via
    /// [`spawn_sig`]): a spawn-baked feature's L2/L3 values must move the
    /// signature so the user gets the restart hint, because a running tab keeps
    /// whatever posture it launched with.
    ///
    /// [`Feature::OpencodeNativeGate`] joins the pair in Phase H: its flag is
    /// baked into the generated OpenCode plugin, and the plugin is written at
    /// tab spawn.
    pub fn spawn_baked(self) -> bool {
        match self {
            Feature::NativeWeb | Feature::ConsumerHygiene | Feature::OpencodeNativeGate => true,
            Feature::TaintLatch
            | Feature::Spotlighting
            | Feature::Detection
            | Feature::SsrfGuard
            | Feature::FetchBudgets
            | Feature::Canary
            | Feature::MemoryQuarantine
            | Feature::TerminalEscapeHygiene => false,
        }
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
    /// No narrower scope applies: an app-wide feature, or a call that carries no
    /// tab identity.
    ///
    /// **Both readings share one variant, so its resolution has to be safe for
    /// the second one** (#48, N-1). An app-wide feature genuinely has no
    /// narrower answer; an identity-less *call* does — the caller is some tab,
    /// we just cannot tell which. So this scope resolves to the app-wide answer
    /// **plus every L3 `On` any configured tab states**: see [`decide`]. The
    /// elevation is one-directional (an L3 `Off` never travels up), so it can
    /// only ever add protection.
    App,
}

impl<'a> Scope<'a> {
    /// The scope for a consumer-side call: the tab when the child sent an
    /// identity, [`Scope::App`] otherwise.
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
    pub fn for_tab(agent: &'a str, tab: Option<&'a str>) -> Self {
        match tab.map(str::trim).filter(|t| !t.is_empty()) {
            Some(tab) => Scope::Tab { agent, tab },
            None => Scope::App,
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
    pub fn key(&self) -> String {
        match self {
            Scope::Tab { agent, tab } => format!("{agent}:{tab}"),
            Scope::OffloadWorker => WORKER_SCOPE_KEY.to_string(),
            Scope::App => APP_SCOPE_KEY.to_string(),
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
/// an untouched config deserializes to all-`Inherit`, i.e. exactly today's
/// behaviour.
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
    /// V32 Phase H. An `On` here is the per-tab way to enable the gate over its
    /// app-wide default `off` — the shape locked decision 17 expects most users
    /// to reach for first (one hardened OpenCode tab, everything else as it was).
    pub(in crate::settings) opencode_native_gate: Override,
}

impl TabInjectionOverrides {
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
    /// At [`Scope::App`] it means something slightly wider — "a narrower scope's
    /// `On` is being honoured here", the identity-less elevation on [`decide`]
    /// (#48, N-1). The app scope has no cell of its own, so there is no other
    /// reading of `scope` available there, and the honest alternative
    /// ([`DecidedBy::Feature`]) would claim L2 said `on` when it said `off`.
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
/// L2 — which is unchanged. Two subtleties in the code below:
///
/// - [`Feature::NativeWeb`]'s L2 is derived from the tri-mode rather than stored
///   as a boolean ([`native_web_l2`]);
/// - at [`Scope::App`] an L3 `On` stated by **any** configured tab is honoured
///   (#48, N-1). That scope stands in for two different questions — "what is the
///   app-wide answer" and "what applies to a call that sent no `--tab`" — and
///   the second one has a tab behind it that we simply cannot name. Only `On`
///   travels up: an L3 `Off` stays where the user put it, so this can never
///   remove protection, only add it to a caller with no identity. It never
///   applies to a scope that HAS an answer (a known tab, the worker), so the
///   locked resolution order for a known scope does not move.
pub fn decide(feature: Feature, scope: Scope<'_>, s: &Settings) -> Decision {
    let inj = &s.offload.injection;
    if !inj.protection {
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
        // N-1: the app scope has no cell of its own, so this is where the
        // identity-less reading gets its answer, BEFORE falling through to L2.
        Override::Inherit if matches!(scope, Scope::App) && any_tab_override_on(feature, s) => {
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
/// identity-less elevation described on [`Scope::App`] and [`Scope::for_tab`]
/// (#48, N-1).
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
fn scope_override(feature: Feature, scope: Scope<'_>, s: &Settings) -> Override {
    match scope {
        Scope::App => Override::Inherit,
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
    if !effective(Feature::Detection, scope, s) {
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
    /// The consumer a tab whose command is `command` launches.
    pub fn for_command(command: &str) -> Self {
        if crate::tabs::config::command_is(command, "claude") {
            Consumer::Claude
        } else {
            Consumer::Opencode
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
            // Phase H's gate exists only inside the generated OpenCode plugin.
            // This is the row the shared blob was nagging Claude tabs about.
            Feature::OpencodeNativeGate => self == Consumer::Opencode,
            // Live features never reach a spawn signature at all — filtered out
            // by `Feature::spawn_baked` before this is asked — but naming them
            // keeps the match total.
            Feature::TaintLatch
            | Feature::Spotlighting
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
/// change.
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
    let rows: Vec<serde_json::Value> = s
        .tabs
        .iter()
        .filter_map(|t| match t {
            TabConfig::AiTool(c) if Consumer::for_command(&c.command) == consumer => Some(c),
            _ => None,
        })
        .map(|c| {
            let scope = Scope::tab_only(&c.id);
            let resolved: Vec<serde_json::Value> = features
                .iter()
                .map(|f| serde_json::json!([f.key(), effective(*f, scope, s)]))
                .collect();
            serde_json::json!([c.id, native_web_mode(s, scope).as_str(), resolved])
        })
        .collect();
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
        Scope::App => !feature.has_tab_scope() && !feature.has_worker_scope(),
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
            }
        })
        .collect()
}

/// Whether protection is REDUCED anywhere the user can see — the master is off,
/// or any feature that ships ON resolves off at a scope that has a row for it.
///
/// **"Reduced" is measured against each feature's default**
/// ([`Feature::default_enabled`]), not against `true`. Phase H's OpenCode native
/// gate ships off by user decision, so a fresh install must not report reduced
/// protection; and a scope that switches that gate ON has more protection than
/// the default, never less.
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
    let any_off = |scope: Scope<'_>| {
        Feature::ALL.iter().any(|f| {
            f.default_enabled() && feature_in_scope(*f, scope) && !effective(*f, scope, s)
        })
    };
    if any_off(Scope::App) || any_off(Scope::OffloadWorker) {
        return true;
    }
    s.tabs.iter().any(|t| match t {
        TabConfig::AiTool(c) => any_off(Scope::Tab {
            agent: "",
            tab: &c.id,
        }),
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
    fn settings() -> Settings {
        Settings {
            tabs: vec![super::super::schema::default_claude_tab()],
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
    /// [`Scope::App`] and **no other feature's**. `set_l2_for_test`'s exhaustive
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
            for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::App] {
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
        let default_off: Vec<_> = Feature::ALL
            .iter()
            .filter(|f| !f.default_enabled())
            .collect();
        assert_eq!(default_off, vec![&Feature::OpencodeNativeGate]);
        // And a fresh install does NOT report reduced protection because of it.
        assert!(!protection_reduced(&s));
    }

    /// V32 Phase H: the default-off feature, end to end through the hierarchy.
    ///
    /// The interesting property is the one that made `default_enabled` necessary:
    /// a control that ships off, and is off, is the BASELINE — it must not light
    /// the reduced-protection indicator — while switching it on is *more*
    /// protection and must not light it either.
    #[test]
    fn the_opencode_native_gate_ships_off_without_reading_as_reduced_protection() {
        let mut s = settings();
        let id = a_tab(&s);
        let f = Feature::OpencodeNativeGate;
        assert!(!f.default_enabled());
        assert!(f.spawn_baked(), "its flag is baked into the plugin");
        assert!(f.has_tab_scope());
        assert!(!f.has_worker_scope(), "the worker is not a harness");

        // Default: off everywhere, and nothing reads as reduced.
        assert!(!effective(f, tab_scope(&id), &s));
        assert!(!protection_reduced(&s));

        // An L3 `On` over the app-wide `off` — the shape decision 17 expects
        // most users to reach for — enables exactly one tab…
        set_tab_override(&mut s, &id, f, Override::On);
        assert!(effective(f, tab_scope(&id), &s));
        assert!(!effective(f, tab_scope("some-other-tab"), &s));
        // …and MORE protection than the default is still not "reduced".
        assert!(!protection_reduced(&s));

        // The app-wide L2 works the same way, and the master still wins.
        let mut s = settings();
        s.offload.injection.opencode_native_gate_enabled = true;
        assert!(effective(f, tab_scope(&id), &s));
        assert!(!protection_reduced(&s));
        s.offload.injection.protection = false;
        assert!(!effective(f, tab_scope(&id), &s));
        assert!(protection_reduced(&s), "the master switch always counts");

        // The report publishes the default so the frontend need not mirror it.
        let rows = report(&settings(), tab_scope(&id));
        let row = rows
            .iter()
            .find(|r| r.feature == "opencode_native_gate")
            .expect("the feature has a row");
        assert!(!row.default_on);
        assert!(!row.effective);
        assert!(row.in_scope);
        assert!(
            rows.iter().filter(|r| !r.default_on).count() == 1,
            "exactly one default-off row"
        );
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
        for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::App] {
            assert_eq!(
                decide(Feature::TaintLatch, scope, &s),
                Decision {
                    effective: false,
                    decided_by: DecidedBy::Global
                }
            );
        }
        // And with the master off EVERY feature is off, at every scope.
        for f in Feature::ALL {
            for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::App] {
                assert!(!effective(*f, scope, &s), "{f:?} at {scope:?}");
            }
        }
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

        let cfg = detection_config(&s, Scope::App);
        assert!(cfg.signature && cfg.classifier);
        s.offload.injection.detection_enabled = false;
        let cfg = detection_config(&s, Scope::App);
        assert!(!cfg.signature && !cfg.classifier);
        // The parent wins over the sub-toggles, in both directions: with the
        // parent off, turning a layer on changes nothing.
        s.offload.detection_signature_enabled = true;
        assert!(!detection_config(&s, Scope::App).signature);
    }

    /// One Claude tab and one OpenCode tab — the shape the per-consumer spawn
    /// signature has to tell apart (#48, F-x).
    fn settings_both_consumers() -> Settings {
        Settings {
            tabs: vec![
                super::super::schema::default_claude_tab(),
                super::super::schema::default_opencode_tab(),
            ],
            ..Settings::default()
        }
    }

    /// The id of the first tab whose command launches `consumer`.
    fn tab_of(s: &Settings, consumer: Consumer) -> String {
        s.tabs
            .iter()
            .find_map(|t| match t {
                TabConfig::AiTool(c) if Consumer::for_command(&c.command) == consumer => {
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
            // V32 Phase H: both levels of the OpenCode native gate. Its flag is
            // compiled into the generated plugin, so a flip that does not move
            // the OpenCode signature is a gate the user believes is on and is
            // not — and one that moves the CLAUDE signature is the F-x nag.
            (
                "opencode-native-gate L2",
                Box::new(|s: &mut Settings| s.set_l2_for_test(Feature::OpencodeNativeGate, true)),
                &[Opencode],
            ),
            (
                "opencode-native-gate L3",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Opencode);
                    set_tab_override(s, &id, Feature::OpencodeNativeGate, Override::On);
                }),
                &[Opencode],
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
                "spotlighting L3",
                Box::new(|s: &mut Settings| {
                    let id = tab_of(s, Claude);
                    set_tab_override(s, &id, Feature::Spotlighting, Override::Off);
                }),
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
    /// indicator — fires for the master switch, an app-wide feature and a single
    /// per-tab override alike.
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
        assert!(protection_reduced(&s), "one tab's override counts");
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
    /// [`Scope::for_tab`] maps a missing `--tab` to [`Scope::App`], documented as
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
        let f = Feature::OpencodeNativeGate; // L2 default off — decision 17's shape.

        // Nothing overridden: the app-wide answer is the L2 default, decided at
        // L2. The elevation must not fire on an untouched config.
        assert_eq!(
            decide(f, Scope::App, &s),
            Decision {
                effective: false,
                decided_by: DecidedBy::Feature
            }
        );

        // One hardened tab. The identity-less call now resolves ON.
        set_tab_override(&mut s, &id, f, Override::On);
        assert_eq!(
            decide(f, Scope::App, &s),
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
            decide(Feature::TaintLatch, Scope::App, &s),
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
            decide(f, Scope::App, &s),
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
                !effective(*feature, Scope::App, &s),
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
            assert_eq!(r.decided_by, DecidedBy::Global, "{}", r.feature);
            assert!(!r.effective);
        }
    }
}

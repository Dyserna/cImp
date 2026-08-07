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
//!   [`Feature`] (default `true`), with the deliberate exception of
//!   [`Feature::NativeWeb`] — see *Native-web reconciliation* below.
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
//! thing. The invariant is pinned by a source-scanning tripwire
//! (`crate::injection_tripwire`), in the style of the Phase D channel-content
//! tripwire: the raw field names may appear only in this module, in the schema
//! that declares them, and in the reviewed per-field allowlist there.
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
//! [`Feature::spawn_baked`] names the two features applied when a tab launches
//! ([`Feature::NativeWeb`], [`Feature::ConsumerHygiene`]). Their L2 *and* L3
//! values ride `tabs::config::spawn_inject_sig` via [`spawn_sig`], so flipping
//! either raises the restart hint. Every other feature resolves per call and
//! takes effect immediately.

use serde::{Deserialize, Serialize};

use super::schema::{Settings, TabConfig};

// ── Features ───────────────────────────────────────────────────────────────

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
    /// Stripping terminal control sequences out of external text cImp composes
    /// into non-HTML sinks. App-wide: TTS and toasts are global surfaces.
    TerminalEscapeHygiene,
}

impl Feature {
    /// Every feature, in the order the Settings UI and the introspection report
    /// render them (cheapest/most-structural first, spawn-baked last).
    pub const ALL: &'static [Feature] = &[
        Feature::TaintLatch,
        Feature::Spotlighting,
        Feature::Detection,
        Feature::SsrfGuard,
        Feature::FetchBudgets,
        Feature::Canary,
        Feature::MemoryQuarantine,
        Feature::NativeWeb,
        Feature::ConsumerHygiene,
        Feature::TerminalEscapeHygiene,
    ];

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
            Feature::TerminalEscapeHygiene => "Terminal escape hygiene",
        }
    }

    /// Whether this feature carries a per-TAB L3 row. Mirrors
    /// [`TabInjectionOverrides`]'s field set (pinned by a test).
    pub fn has_tab_scope(self) -> bool {
        matches!(
            self,
            Feature::TaintLatch
                | Feature::Spotlighting
                | Feature::Detection
                | Feature::SsrfGuard
                | Feature::FetchBudgets
                | Feature::MemoryQuarantine
                | Feature::NativeWeb
                | Feature::ConsumerHygiene
        )
    }

    /// Whether this feature carries the `offload-worker` L3 row. Mirrors
    /// [`WorkerInjectionOverrides`]'s field set (pinned by a test).
    ///
    /// Memory quarantine is deliberately absent: the worker cannot dispatch
    /// `context_*` at all (issue #38) and serves a hard refusal instead, so a
    /// worker quarantine row would be a switch with no enforcement site behind
    /// it. Native-web visibility and consumer hygiene are absent because the
    /// worker is not a harness — it has no native tools and no spawn config.
    pub fn has_worker_scope(self) -> bool {
        matches!(
            self,
            Feature::TaintLatch
                | Feature::Spotlighting
                | Feature::Detection
                | Feature::SsrfGuard
                | Feature::FetchBudgets
                | Feature::Canary
        )
    }

    /// Whether this feature is applied at TAB SPAWN rather than per call.
    ///
    /// The consumer of this predicate is `tabs::config::spawn_inject_sig` (via
    /// [`spawn_sig`]): a spawn-baked feature's L2/L3 values must move the
    /// signature so the user gets the restart hint, because a running tab keeps
    /// whatever posture it launched with.
    pub fn spawn_baked(self) -> bool {
        matches!(self, Feature::NativeWeb | Feature::ConsumerHygiene)
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
    /// tab identity (the same fail-open shape the latch takes, resolved here as
    /// "the app-wide answer" rather than as "off").
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
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

impl From<String> for Override {
    fn from(s: String) -> Self {
        Override::parse(&s)
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
    pub taint_latch: Override,
    pub spotlighting: Override,
    pub detection: Override,
    pub ssrf_guard: Override,
    pub fetch_budgets: Override,
    pub memory_quarantine: Override,
    pub native_web: Override,
    pub consumer_hygiene: Override,
}

impl TabInjectionOverrides {
    /// This row's cell for `feature`, or [`Override::Inherit`] for a feature
    /// that has no tab scope — the honest answer, since there is no cell to
    /// read and inheriting is what "no override" means.
    pub fn get(&self, feature: Feature) -> Override {
        match feature {
            Feature::TaintLatch => self.taint_latch,
            Feature::Spotlighting => self.spotlighting,
            Feature::Detection => self.detection,
            Feature::SsrfGuard => self.ssrf_guard,
            Feature::FetchBudgets => self.fetch_budgets,
            Feature::MemoryQuarantine => self.memory_quarantine,
            Feature::NativeWeb => self.native_web,
            Feature::ConsumerHygiene => self.consumer_hygiene,
            Feature::Canary | Feature::TerminalEscapeHygiene => Override::Inherit,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Set one cell, for the Settings IPC. `None` for a feature with no tab
    /// scope — the caller is asking for a cell that does not exist, and
    /// silently dropping the write would look like it worked.
    pub fn set(&mut self, feature: Feature, value: Override) -> Option<()> {
        match feature {
            Feature::TaintLatch => self.taint_latch = value,
            Feature::Spotlighting => self.spotlighting = value,
            Feature::Detection => self.detection = value,
            Feature::SsrfGuard => self.ssrf_guard = value,
            Feature::FetchBudgets => self.fetch_budgets = value,
            Feature::MemoryQuarantine => self.memory_quarantine = value,
            Feature::NativeWeb => self.native_web = value,
            Feature::ConsumerHygiene => self.consumer_hygiene = value,
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
    pub taint_latch: Override,
    pub spotlighting: Override,
    pub detection: Override,
    pub ssrf_guard: Override,
    pub fetch_budgets: Override,
    pub canary: Override,
}

impl WorkerInjectionOverrides {
    /// This row's cell for `feature`, or [`Override::Inherit`] for a feature
    /// with no worker scope.
    pub fn get(&self, feature: Feature) -> Override {
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
            | Feature::TerminalEscapeHygiene => Override::Inherit,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Set one cell, for the Settings IPC. `None` for a feature with no worker
    /// scope (see [`TabInjectionOverrides::set`]).
    pub fn set(&mut self, feature: Feature, value: Override) -> Option<()> {
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
/// See the module docs for the locked rule. The only subtlety in the code below
/// is that [`Feature::NativeWeb`]'s L2 is derived from the tri-mode rather than
/// stored as a boolean — [`native_web_l2`].
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
        Override::Inherit => Decision {
            effective: feature_l2(feature, s),
            decided_by: DecidedBy::Feature,
        },
    }
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
/// in the crate that read these raw fields** — the invariant the tripwire pins.
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

/// The hierarchy's contribution to `tabs::config::spawn_inject_sig`.
///
/// Every level of every **spawn-baked** feature ([`Feature::spawn_baked`]), for
/// every AI tab: the master switch, the app-wide L2 inputs, and each tab's
/// resolved values. A flip at any of the three levels moves this, so the user
/// gets the restart hint a spawn-baked change owes them.
///
/// Live features are deliberately absent: they take effect on the next call, and
/// a restart nag for a change that needs no restart is how a hint stops being
/// read.
///
/// It lives here rather than in `tabs::config` because it is the only other
/// thing that has to look at the raw switches, and the no-raw-reads invariant is
/// worth more than the locality.
///
/// The resolved native-web MODE rides alongside the switches deliberately:
/// `sensor` and `deny` both resolve the feature "on" but launch a tab very
/// differently, so a signature built from booleans alone would miss a mode
/// change.
pub fn spawn_sig(s: &Settings) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = s
        .tabs
        .iter()
        .filter_map(|t| match t {
            TabConfig::AiTool(c) => Some(c),
            _ => None,
        })
        .map(|c| {
            let scope = Scope::tab_only(&c.id);
            // Driven by `Feature::spawn_baked` rather than by a hand-written
            // pair, so a future spawn-baked control gets its restart hint by
            // declaring itself — not by someone remembering this function.
            let resolved: Vec<serde_json::Value> = Feature::ALL
                .iter()
                .filter(|f| f.spawn_baked())
                .map(|f| serde_json::json!([f.key(), effective(*f, scope, s)]))
                .collect();
            serde_json::json!([c.id, native_web_mode(s, scope).as_str(), resolved])
        })
        .collect();
    serde_json::json!({
        // L1 explicitly, even though it is folded into every resolved value
        // above: a master flip on an install with no AI tabs configured must
        // still move the signature rather than compare equal to itself.
        "master": s.offload.injection.protection,
        // The app-wide L2 inputs for the two spawn-baked features. The mode is
        // native-web's L2 — see the module docs' reconciliation note.
        "l2": [
            s.offload.native_web_visibility,
            s.offload.injection.consumer_hygiene_enabled,
        ],
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
                in_scope: match scope {
                    Scope::Tab { .. } => f.has_tab_scope(),
                    Scope::OffloadWorker => f.has_worker_scope(),
                    Scope::App => !f.has_tab_scope() && !f.has_worker_scope(),
                },
            }
        })
        .collect()
}

/// Whether protection is REDUCED anywhere the user can see — the master is off,
/// or any feature resolves off at the app scope or at any scope with a row.
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
/// arguing about.
pub fn master_enabled(s: &Settings) -> bool {
    s.offload.injection.protection
}

pub fn protection_reduced(s: &Settings) -> bool {
    if !s.offload.injection.protection {
        return true;
    }
    let any_off = |scope: Scope<'_>| report(s, scope).iter().any(|r| r.in_scope && !r.effective);
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
        for t in &mut s.tabs {
            if let TabConfig::AiTool(c) = t {
                if c.id == id {
                    c.injection_overrides.set(feature, value);
                }
            }
        }
    }

    /// The migration-safety test: an untouched config resolves EVERY feature ON
    /// at every scope — i.e. exactly the pre-Phase-G behaviour, at every layer
    /// at once.
    #[test]
    fn an_untouched_config_resolves_every_feature_on() {
        let s = settings();
        let id = a_tab(&s);
        for f in Feature::ALL {
            for scope in [tab_scope(&id), Scope::OffloadWorker, Scope::App] {
                let d = decide(*f, scope, &s);
                assert!(d.effective, "{:?} at {:?} must default ON", f, scope);
                assert_eq!(d.decided_by, DecidedBy::Feature, "{f:?}");
            }
        }
        assert!(!protection_reduced(&s));
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
        s.offload.injection.worker.canary = Override::Off;
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

    /// The spawn signature moves for spawn-baked features at BOTH levels, and
    /// stays put for the live ones.
    #[test]
    fn the_spawn_signature_tracks_only_spawn_baked_levels() {
        let base = settings();
        // `spawn_sig` walks every AI tab itself, so no id is needed here.
        let _ = a_tab(&base);
        let sig = spawn_sig;
        let before = sig(&base);

        for (name, mutate) in [
            (
                "native-web L2 (the mode)",
                Box::new(|s: &mut Settings| s.offload.native_web_visibility = "deny".into())
                    as Box<dyn Fn(&mut Settings)>,
            ),
            (
                "native-web L3",
                Box::new(|s: &mut Settings| {
                    let id = a_tab(s);
                    set_tab_override(s, &id, Feature::NativeWeb, Override::Off);
                }),
            ),
            (
                "consumer-hygiene L2",
                Box::new(|s: &mut Settings| s.offload.injection.consumer_hygiene_enabled = false),
            ),
            (
                "consumer-hygiene L3",
                Box::new(|s: &mut Settings| {
                    let id = a_tab(s);
                    set_tab_override(s, &id, Feature::ConsumerHygiene, Override::Off);
                }),
            ),
            (
                "the global master",
                Box::new(|s: &mut Settings| s.offload.injection.protection = false),
            ),
        ] {
            let mut s = settings();
            mutate(&mut s);
            assert_ne!(sig(&s), before, "{name} must move the spawn signature");
        }

        // Live features must NOT move it — a restart hint for a change that
        // takes effect on the next call is how a hint stops being read.
        for (name, mutate) in [
            (
                "taint latch L2",
                Box::new(|s: &mut Settings| s.offload.injection.taint_latch_enabled = false)
                    as Box<dyn Fn(&mut Settings)>,
            ),
            (
                "spotlighting L3",
                Box::new(|s: &mut Settings| {
                    let id = a_tab(s);
                    set_tab_override(s, &id, Feature::Spotlighting, Override::Off);
                }),
            ),
            (
                "detection L2",
                Box::new(|s: &mut Settings| s.offload.injection.detection_enabled = false),
            ),
            (
                "fetch budgets L2",
                Box::new(|s: &mut Settings| s.offload.injection.fetch_budgets_enabled = false),
            ),
        ] {
            let mut s = settings();
            mutate(&mut s);
            assert_eq!(sig(&s), before, "{name} must not move the spawn signature");
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
        s.offload.injection.worker.canary = Override::Off;
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
        assert_eq!(seen.len(), Feature::ALL.len());
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

//! V40 Phase A — **the harness registry**: one production descriptor per
//! harness, and the ONE place that answers "which harness is this?".
//!
//! Locked decision 1. Before this module the question had ten different
//! answers, in ten different vocabularies, six of which fell back to Claude for
//! an unrecognised command — so a third harness was not rejected, it was
//! *misattributed*: wrong activity badge, wrong injection scope, wrong expose
//! flag, wrong audit latch slot, wrong graph source, and nothing said so.
//!
//! # The shape, and why it is this shape
//!
//! [`HarnessId`] is **opaque**. It carries the registry's own id string and
//! nothing else, it can only be produced by a lookup in this module, and it has
//! no `Claude` / `OpenCode` constants for core to compare against — which is
//! what makes locked decision 10(a) (*"core may hold a `HarnessId` and pass it
//! to the registry; it may not branch on its value"*) checkable rather than
//! aspirational. Adding a harness is a [`HarnessDescriptor`] row plus a
//! `harness/<id>/` directory; it is deliberately **not** a new enum variant,
//! because a variant is a thing every `match` in the tree can be made to care
//! about.
//!
//! [`HarnessId::ANY`] is the one non-harness value: the harness-neutral marker
//! for registry rows whose contract is stated about a *tab* rather than about a
//! vendor (`delegation.worker` is the first, V39 Phase B). It is not a
//! descriptor, has no directory, no binary and no plugin, and every lookup that
//! wants a real product refuses it.
//!
//! # `PerHarness<T>`
//!
//! Locked decision 25. Every fixed-arity-2 structure in the tree — the audit
//! consumer list, the unscoped ledger's slots, the per-server access pair, the
//! `tool_defs_for_{claude,opencode}` pair — was a place where adding a harness
//! meant finding and widening an array literal, and where forgetting to do so
//! compiled. [`PerHarness`] is sized by the registry, so the same mistake is a
//! type error.

use std::collections::BTreeMap;

use super::plugin::HarnessPlugin;

// ── the id ──────────────────────────────────────────────────────────────────

/// A registered harness, or [`HarnessId::ANY`].
///
/// Opaque by construction: the inner string is this module's, and the only ways
/// to obtain one are the lookups below. See the module docs for why there are
/// no `HarnessId::Claude`-style constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HarnessId(&'static str);

/// The token [`HarnessId::ANY`] prints as, on the wire and in reports.
const ANY_TOKEN: &str = "any";

impl HarnessId {
    /// Harness-neutral — served by whatever adapter is attached.
    ///
    /// **Constructed since V39 Phase B**, by exactly one capability row
    /// (`delegation.worker`, locked decision 16): a requirement stated about a
    /// tab, not about a vendor.
    pub const ANY: HarnessId = HarnessId("");

    /// Declare an id at a **compile-time** site inside `harness/` — the
    /// descriptor table and the capability registry's `harness:` column.
    ///
    /// Unvalidated by construction (a `const fn` cannot search a slice), which
    /// is why `registry::every_declared_id_is_registered` checks every one of
    /// them and why this is not visible outside `harness/`: core obtains a
    /// `HarnessId` from a lookup that can fail, never from a literal.
    pub(in crate::harness) const fn declared(id: &'static str) -> HarnessId {
        HarnessId(id)
    }

    /// The CHP `agent` discriminator — `None` for [`Self::ANY`], which names no
    /// vendor.
    pub fn id(self) -> Option<&'static str> {
        (self != Self::ANY).then_some(self.0)
    }

    /// The report/wire token, which unlike [`Self::id`] has a spelling for
    /// [`Self::ANY`] (`"any"`). The `--harness-canary` report, its `--json`
    /// twin and the *Harness health* payload all speak this vocabulary.
    pub fn token(self) -> &'static str {
        if self == Self::ANY {
            ANY_TOKEN
        } else {
            self.0
        }
    }

    /// What a human (and a generated tool description) calls this harness.
    pub fn label(self) -> &'static str {
        match self.descriptor() {
            Some(d) => d.label,
            None => "any harness",
        }
    }

    /// This harness's descriptor — `None` for [`Self::ANY`].
    pub fn descriptor(self) -> Option<&'static HarnessDescriptor> {
        descriptors().find(|d| d.id == self.0)
    }

    /// This harness's plugin — `None` for [`Self::ANY`].
    pub fn plugin(self) -> Option<&'static dyn HarnessPlugin> {
        self.descriptor().map(|d| d.plugin)
    }

    /// Registry position, the key [`PerHarness`] indexes by. `None` for
    /// [`Self::ANY`].
    pub fn ordinal(self) -> Option<usize> {
        HARNESSES.iter().position(|d| d.id == self.0)
    }

    /// The harness whose CHP agent id is `id`.
    ///
    /// **Refuses `"any"`** — it names no installed product, so there is nothing
    /// to drive, probe or spawn. Callers that legitimately want the neutral
    /// marker name [`Self::ANY`] directly.
    pub fn from_id(id: &str) -> Option<HarnessId> {
        descriptors().find(|d| d.id == id).map(|d| HarnessId(d.id))
    }

    /// The harness a configured command launches, comparing on the file stem so
    /// `"claude"`, `"claude.exe"` and `"/usr/bin/claude"` all resolve.
    ///
    /// **`None` is a first-class answer** (locked decision 2): a tab whose
    /// command is neither is a *shell tab*, not a Claude tab. Six sites used to
    /// answer Claude here and every one of them was a silent misattribution.
    ///
    /// **Both separators are split, on every platform.** The command is a value
    /// out of a settings file, not a path this build produced, so a Windows
    /// spelling (`C:\bin\claude.exe`) can reach a Linux build through a synced
    /// or hand-edited `settings.json` — and `Path::file_stem` would hand back
    /// the whole string there, classifying a Claude tab as a shell tab.
    pub fn from_command(command: &str) -> Option<HarnessId> {
        let file = command.rsplit(['/', '\\']).next().unwrap_or(command);
        let stem = std::path::Path::new(file).file_stem().and_then(|s| s.to_str())?;
        descriptors()
            .find(|d| d.binaries.iter().any(|b| b.eq_ignore_ascii_case(stem)))
            .map(|d| HarnessId(d.id))
    }

    /// The harness that owns a reserved built-in tab id (`claude`,
    /// `claude-local`, `opencode`). `None` for a user-created `ai-<uuid>` tab —
    /// those are classified by their command, not by their id.
    pub fn from_tab_id(tab_id: &str) -> Option<HarnessId> {
        descriptors()
            .find(|d| d.tabs.iter().any(|t| t.id == tab_id))
            .map(|d| HarnessId(d.id))
    }

    /// The harness behind an MCP consumer token.
    ///
    /// `None` for a token nobody declared — which is a refusal, not a default:
    /// `cimp --consumer codex` must fail the proxy start with the registered
    /// list rather than silently serving Claude's tool set.
    /// **Padding is not trimmed**, deliberately. This is the lookup
    /// `graph::source_for_consumer` runs on a CALLER-ASSERTED wire value, and
    /// narrowing `" opencode "` to a harness there would be a widening dressed
    /// as a convenience — the narrowing that IS wanted lives at the routes that
    /// require identity (`audit_consumer`, `resolve_consumer`), which trim
    /// before they ask.
    pub fn from_consumer(consumer: &str) -> Option<HarnessId> {
        descriptors()
            .find(|d| d.consumer.eq_ignore_ascii_case(consumer))
            .map(|d| HarnessId(d.id))
    }
}

impl std::fmt::Display for HarnessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.token())
    }
}

// ── the descriptor ──────────────────────────────────────────────────────────

/// A capability a harness declares that core mounts richer UI or wiring for.
///
/// Declared rather than inferred from `id == "claude"` (locked decision 6): a
/// feature only one harness has today is still a *feature*, and the next
/// harness that grows one must get it by saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumers land in phases B/D — the declarations are Phase A's
pub enum HarnessFeature {
    /// Per-turn token/quota accounting cImp can read (Claude's transcript
    /// `message.usage`). Mounts the session-usage panels.
    SessionUsage,
    /// A context-window reading cImp can render as a meter.
    ContextBar,
    /// The harness is configured through a **file cImp writes at spawn** — so
    /// it has plugin goldens, and a stale artifact is detectable.
    FileArtifact,
    /// A tab of this harness PUSHES a status-line reading into cImp's usage
    /// file as it runs, so the bottom-bar widget has something to poll for.
    ///
    /// Distinct from [`Self::SessionUsage`], which is about the transcript cImp
    /// reads *after* a turn: this one is about whether a running tab feeds the
    /// live widget, and it is the question `contextMeter`'s hand-written
    /// `commandIsClaude` used to answer (locked decision 19). Declaring it
    /// without a [`super::plugin::HarnessPlugin::usage_source`] is refused by
    /// `a_declared_usage_push_has_a_source`.
    UsagePush,
    /// cImp can WRITE this harness's local-provider configuration for it — the
    /// plugin has a [`super::plugin::ConfigWriter`].
    ///
    /// What mounts the Offload card's *register this backend with the harness*
    /// button. Declared rather than inferred, and cross-checked against the
    /// plugin by `a_declared_config_writer_exists`: a button that derives a
    /// provider block for a harness with no writer is a click that can only
    /// fail.
    LocalProviderConfig,
}

/// One reserved built-in tab a harness owns — **everything core needs to seed
/// that tab without naming the product** (V40 Phase I, issue #107 item 1).
///
/// Before this, `tab_ids: &[&str]` said only that the id existed; the tab's
/// name, its command, its notification prose and its `use_local_provider` flag
/// lived in three hand-written `default_*_tab()` constructors in
/// `settings::schema` plus a `match` per tab id. That match is exactly what
/// locked decision 3's residual was about: a third harness's tab compiled, and
/// then there was no arm to seed it from.
///
/// The fields are the *only* places the three shipped defaults differed. Every
/// other field of the seeded `AiToolTabConfig` is the same for all of them, so
/// [`crate::settings::default_ai_tab`] is now one constructor over this row —
/// and its output for `claude` / `claude-local` / `opencode` is byte-identical
/// to what the three hand-written constructors produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinTab {
    /// The reserved tab id — a **persisted wire form** (locked decision 29).
    /// It is in every settings file's `tabs[]`, `enabled_ai_tabs` and layout
    /// panes, and in every frontend payload.
    pub id: &'static str,
    /// The tab-bar name a freshly seeded tab carries, and the noun each of its
    /// four default notification slots is phrased around ("<name> is idle").
    pub name: &'static str,
    /// The command a freshly seeded tab launches. A wire form too: it is what
    /// lands in the user's settings file, editable from there afterwards.
    pub command: &'static str,
    /// Seeded `use_local_provider` — whether this tab is the harness's
    /// local-provider variant. `claude-local` is the only `true` today.
    pub local_provider: bool,
}

/// One harness, as data. All `'static`; no I/O.
///
/// Every field here is something core used to ask a bespoke function for. The
/// rule for what belongs: *would this still make sense if both shipped
/// harnesses were deleted?* If not, it is a descriptor field or a
/// [`HarnessPlugin`] method — never a branch in core.
pub struct HarnessDescriptor {
    /// The CHP `agent` discriminator and the `harness/<id>/` directory name.
    pub id: &'static str,
    /// What a human calls it.
    pub label: &'static str,
    /// The binaries whose file stem identifies this harness.
    pub binaries: &'static [&'static str],
    /// The reserved built-in tabs, **in canonical order** — the order
    /// `restore_enabled_ai_builtins` and the tab-lifecycle inserter place them
    /// in, so a re-enabled built-in lands where the user expects it.
    pub tabs: &'static [BuiltinTab],
    /// The MCP consumer token the per-session child is launched with.
    pub consumer: &'static str,
    /// Whether a tab of this harness is expected to speak CHP — i.e. whether a
    /// spawn-baked artifact of ours states its protocol version, and therefore
    /// whether staleness detection covers it (V35 Phase I locked decision 5).
    pub expects_chp: bool,
    /// Environment markers of a session of THIS harness that cImp was launched
    /// from, stripped from every AI tab's child.
    pub env_strip: &'static [&'static str],
    /// What core mounts for this harness beyond the neutral path.
    ///
    /// Read by `every_registry_entry_is_fully_wired` today; the production
    /// consumers (the Settings sections, the `harness_list` IPC mirror) land
    /// with locked decisions 6 and 7 in phases B and D. Declared here in Phase A
    /// because the DECLARATION is what those phases consume — a feature
    /// inferred from `id == "claude"` later is the thing this milestone exists
    /// to prevent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub features: &'static [HarnessFeature],
    /// The code half. See [`HarnessPlugin`].
    pub plugin: &'static dyn HarnessPlugin,
}

impl HarnessDescriptor {
    /// This descriptor's id as a [`HarnessId`].
    pub fn harness(&'static self) -> HarnessId {
        HarnessId(self.id)
    }

    /// This harness's reserved tab ids, in canonical order.
    pub fn tab_ids(&'static self) -> impl Iterator<Item = &'static str> {
        self.tabs.iter().map(|t| t.id)
    }
}

impl std::fmt::Debug for HarnessDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessDescriptor")
            .field("id", &self.id)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// The registry. **One entry per `harness/<id>/` directory** — asserted in both
/// directions by `layering::every_registry_entry_is_fully_wired`.
///
/// A `const` (re-exported as [`HARNESSES`]) so [`COUNT`] is available in const
/// context, which is what lets [`PerHarness`] be a fixed-size array sized by
/// the registry instead of a hand-written `2`.
const REGISTRY: &[HarnessDescriptor] = &[
    HarnessDescriptor {
        id: "claude",
        label: "Claude Code",
        binaries: &["claude"],
        // `claude-local` is the same binary with a synthesized provider env, so
        // it is the same harness with the same state directories.
        tabs: &[
            BuiltinTab {
                id: "claude",
                name: "Claude",
                command: "claude",
                local_provider: false,
            },
            // V1.4-07: the same binary, preconfigured to talk to a local LLM
            // through the `claude_local` provider settings.
            BuiltinTab {
                id: "claude-local",
                name: "Claude (local)",
                command: "claude",
                local_provider: true,
            },
        ],
        consumer: "claude",
        // Since V35 Phase J: Claude's `type: "http"` hook entries carry
        // `X-CIMP-Chp`, substituted from `CHP_VERSION` at overlay generation,
        // so a Claude tab IS a spawn-baked artifact that states its version.
        expects_chp: true,
        // V30 (review M9). The load-bearing one is `CLAUDE_CODE_CHILD_SESSION`:
        // a Claude spawned with it set runs with no transcript, no history and
        // no session records, which silently blinds the out-of-band tap. The
        // other two are the generic "you are running inside Claude Code"
        // markers a fresh, user-facing tab must not claim to be under.
        env_strip: &[
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
        ],
        features: &[
            HarnessFeature::SessionUsage,
            HarnessFeature::ContextBar,
            HarnessFeature::UsagePush,
        ],
        plugin: &super::claude::PLUGIN,
    },
    HarnessDescriptor {
        id: "opencode",
        label: "OpenCode",
        binaries: &["opencode"],
        // V19: OpenCode picks its own provider/model, so (unlike Claude) there
        // is no local variant — the one tab covers cloud and local.
        tabs: &[BuiltinTab {
            id: "opencode",
            name: "OpenCode",
            command: "opencode",
            local_provider: false,
        }],
        consumer: "opencode",
        expects_chp: true,
        env_strip: &[],
        features: &[
            HarnessFeature::FileArtifact,
            HarnessFeature::LocalProviderConfig,
        ],
        plugin: &super::opencode::PLUGIN,
    },
];

/// Every registered harness, in declaration order.
pub static HARNESSES: &[HarnessDescriptor] = REGISTRY;

/// How many harnesses are registered. `const`, so [`PerHarness`] is sized by
/// the registry rather than by a literal somebody has to remember to widen.
pub const COUNT: usize = REGISTRY.len();

/// The descriptors an **identity lookup** sees: the shipped registry, plus the
/// scoped test descriptor when one is installed.
///
/// V40 Phase I. Locked decision 1 says adding a harness is a data row — and the
/// only honest proof of that is a test that adds one and drives the real
/// production functions with it. The extra slot is `#[cfg(test)]`-only,
/// thread-scoped (so it perturbs nothing running in parallel) and empty unless
/// [`with_extra_harness`] installed it; in a release build this is
/// `HARNESSES.iter().chain(None)`, which is the bare iterator.
///
/// **Identity only.** [`HarnessId::ordinal`], [`all`] and therefore
/// [`PerHarness`] deliberately stay on [`HARNESSES`]: `PerHarness` is a
/// `[T; COUNT]` sized at compile time, and a lookup that could hand it an
/// out-of-range ordinal would turn a test fixture into a panic in production
/// code. A scoped extra harness has no `PerHarness` slot, exactly like
/// [`HarnessId::ANY`].
pub(in crate::harness) fn descriptors() -> impl Iterator<Item = &'static HarnessDescriptor> {
    HARNESSES.iter().chain(extra_harness())
}

#[cfg(test)]
thread_local! {
    /// The scoped test descriptor, if [`with_extra_harness`] installed one.
    static EXTRA_HARNESS: std::cell::Cell<Option<&'static HarnessDescriptor>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn extra_harness() -> Option<&'static HarnessDescriptor> {
    EXTRA_HARNESS.with(|c| c.get())
}

#[cfg(not(test))]
fn extra_harness() -> Option<&'static HarnessDescriptor> {
    None
}

/// Run `f` with `extra` visible to every identity lookup and to the tab
/// machinery — **tests only**.
///
/// Restores the previous value on the way out, including on unwind, so a
/// failing assertion inside `f` cannot leak the fixture into the rest of the
/// thread.
#[cfg(test)]
pub(crate) fn with_extra_harness<R>(
    extra: &'static HarnessDescriptor,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<&'static HarnessDescriptor>);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXTRA_HARNESS.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(EXTRA_HARNESS.with(|c| c.replace(Some(extra))));
    f()
}

/// **A harness no build ships** — the fixture behind
/// `harness::layering::an_unshipped_descriptor_round_trips_through_the_tab_machinery`
/// (V40 Phase I).
///
/// Locked decision 1 says adding a harness is a descriptor row plus a
/// `harness/<id>/` directory, and *not* an enum variant. That claim was only
/// ever checked by tests asserting the two shipped harnesses still worked —
/// which is the same evidence a build with three hard-coded enum arms would
/// produce. This row is the counter-example: nothing in the tree has ever seen
/// the id `zeta`, its plugin declares nothing at all (`HarnessPlugin` has no
/// required methods, by design), and the tab machinery has to carry it anyway.
///
/// It is deliberately NOT in [`HARNESSES`]: the shipped registry is what
/// `every_registry_entry_is_fully_wired` requires a directory, a capability row,
/// a hello, a spawn signature and an input profile for, and what [`PerHarness`]
/// is sized by. This one is installed for the length of one closure, on one
/// thread, by [`with_extra_harness`].
#[cfg(test)]
pub(crate) static EXTRA_TEST_HARNESS: HarnessDescriptor = HarnessDescriptor {
    id: "zeta",
    label: "Zeta",
    binaries: &["zeta"],
    tabs: &[BuiltinTab {
        id: "zeta",
        name: "Zeta",
        command: "zeta",
        local_provider: false,
    }],
    consumer: "zeta",
    expects_chp: true,
    env_strip: &[],
    features: &[],
    plugin: &BARE_TEST_PLUGIN,
};

/// The plugin half of [`EXTRA_TEST_HARNESS`]: every method defaulted.
#[cfg(test)]
struct BareTestPlugin;
#[cfg(test)]
impl HarnessPlugin for BareTestPlugin {}
#[cfg(test)]
static BARE_TEST_PLUGIN: BareTestPlugin = BareTestPlugin;

/// Every registered harness id, in declaration order.
pub fn harness_ids() -> Vec<&'static str> {
    HARNESSES.iter().map(|d| d.id).collect()
}

/// Every registered harness, in declaration order.
pub fn all() -> impl Iterator<Item = HarnessId> {
    HARNESSES.iter().map(|d| HarnessId(d.id))
}

/// The plugin-registered subcommand `argv` names, or `None`.
///
/// Locked decision 19/26: `main.rs` used to test for `--statusline` by name,
/// which made one harness's CLI contract a fact about cImp's own entry point.
/// It asks here now, and a harness that needs cImp to answer a flag declares it
/// in [`HarnessPlugin::subcommands`].
///
/// Scans the whole of `argv` rather than only `argv[1]`, because that is what
/// the branch it replaces did (`args().skip(1).any(..)`) — cImp forwards
/// unrecognised arguments to a Claude tab, so a subcommand flag can arrive
/// after other args and must still be recognised.
pub fn subcommand_for(argv: &[String]) -> Option<&'static super::plugin::Subcommand> {
    HARNESSES
        .iter()
        .flat_map(|d| d.plugin.subcommands())
        .find(|sub| argv.iter().any(|a| a == sub.flag))
}

/// The harness cImp is a **drop-in replacement for** — the one whose plugin
/// says [`super::plugin::HarnessPlugin::accepts_passthrough_argv`] — or `None`.
///
/// Locked decision 26. `main.rs` stated the contract in its help text and
/// `tabs::config` enforced it per tab, from two different places; this is the
/// one lookup both use, so the sentence a user reads names the tab the args
/// actually reach.
///
/// **At most one** is a real constraint, not a convention: forwarding one CLI's
/// flags into another harness is how a tab fails to launch, and
/// `at_most_one_harness_takes_the_passthrough_argv` fails the build if two
/// declare it. With two declared this answers `None` rather than picking — the
/// fail-closed direction, where the sentence names no harness instead of the
/// wrong one.
pub fn passthrough_harness() -> Option<HarnessId> {
    let mut takers = HARNESSES
        .iter()
        .filter(|d| d.plugin.accepts_passthrough_argv());
    let first = takers.next()?;
    takers.next().is_none().then_some(HarnessId(first.id))
}

/// Every reserved built-in tab id, in canonical order across harnesses.
///
/// The order `[claude, claude-local, opencode]` used to be a literal array in
/// three places (`persistence.rs`, `ipc/tab_lifecycle.rs`, `tabs/config.rs`);
/// it is now the registry's declaration order flattened through
/// [`HarnessDescriptor::tabs`].
pub fn canonical_tab_ids() -> Vec<&'static str> {
    descriptors().flat_map(|d| d.tab_ids()).collect()
}

/// The declaration behind a reserved built-in tab id, or `None` for an id no
/// harness claims (a shell tab, an `ai-<uuid>` duplicate, a retired id).
///
/// V40 Phase I: the single lookup `TabId::from_str`, `AiTabId` and
/// `default_ai_tab` all resolve a tab id through, so the three cannot disagree
/// about what a reserved id is.
pub fn builtin_tab(tab_id: &str) -> Option<&'static BuiltinTab> {
    descriptors().flat_map(|d| d.tabs.iter()).find(|t| t.id == tab_id)
}

// ── the one named default ───────────────────────────────────────────────────

/// **The one Claude default left in the tree** (locked decision 22).
///
/// Thirteen sites used to spell `unwrap_or("claude")` (and two the opposite),
/// and they were not one decision repeated — they were a *promise to older
/// shim builds*, all of them on the loopback wire. A cImp shim or generated
/// plugin from before the `consumer` / `agent` field existed posts a body
/// without it, and the only build that ever did that was Claude's: OpenCode's
/// plugin has carried `agent` since the field was introduced, and every route
/// that can be reached by a harness at all is reached by an artifact cImp
/// itself generated.
///
/// So this constant does NOT mean "when in doubt, Claude". It means *"a body
/// with no identity on this wire came from a build old enough that Claude was
/// the only thing that could have sent it"* — a compatibility statement with an
/// expiry date, which is why it is one named constant with this comment rather
/// than thirteen literals nobody can grep for a rationale.
///
/// New code does not get to use it. A route that can carry an identity must
/// require one; `HarnessId::from_*` returning `None` is the answer.
pub const DEFAULT_HARNESS: HarnessId = HarnessId("claude");

// ── PerHarness ──────────────────────────────────────────────────────────────

/// One `T` per registered harness, keyed by [`HarnessId::ordinal`].
///
/// Locked decision 25. The point is the *sizing*: `[T; COUNT]` cannot be built
/// without a value for every harness, so "forgot the third slot" is a compile
/// error where `[T; 2]` was a silent hole — which is exactly how
/// `spawn_inject_sig`'s positional pair could disable a safety mechanism
/// without a diff.
///
/// [`HarnessId::ANY`] has no slot on purpose: it names no product, so a
/// per-product value for it would be a lie with a home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerHarness<T>([T; COUNT]);

impl<T: Copy> PerHarness<T> {
    /// Every slot the same value. `const`, so a `static Mutex<PerHarness<_>>`
    /// can be initialised at compile time (the unscoped audit ledger is one).
    pub const fn filled(value: T) -> Self {
        PerHarness([value; COUNT])
    }
}

impl<T> PerHarness<T> {
    /// One slot per harness, computed in registry order.
    pub fn from_fn(mut f: impl FnMut(HarnessId) -> T) -> Self {
        let slots: Vec<T> = HARNESSES.iter().map(|d| f(HarnessId(d.id))).collect();
        // `COUNT == HARNESSES.len()` by construction; `try_into` cannot fail.
        let arr: [T; COUNT] = match slots.try_into() {
            Ok(a) => a,
            Err(_) => unreachable!("PerHarness is sized by the registry it iterates"),
        };
        PerHarness(arr)
    }

    /// This harness's slot. `None` for [`HarnessId::ANY`].
    pub fn get(&self, id: HarnessId) -> Option<&T> {
        id.ordinal().map(|i| &self.0[i])
    }

    /// This harness's slot, mutably. `None` for [`HarnessId::ANY`].
    #[allow(dead_code)] // the writers land with the settings map in Phase B
    pub fn get_mut(&mut self, id: HarnessId) -> Option<&mut T> {
        id.ordinal().map(move |i| &mut self.0[i])
    }

    /// Every `(harness, value)` pair, in registry order.
    pub fn iter(&self) -> impl Iterator<Item = (HarnessId, &T)> {
        HARNESSES
            .iter()
            .zip(self.0.iter())
            .map(|(d, v)| (HarnessId(d.id), v))
    }

}

/// Slot access by **registry ordinal**, for a consumer that already holds one
/// (a test naming "the first harness's half", an ordinal-keyed ledger).
///
/// Deliberately NOT how production code reads a harness's value — that is
/// [`PerHarness::get`], keyed by [`HarnessId`], which cannot be off by one.
impl<T> std::ops::Index<usize> for PerHarness<T> {
    type Output = T;
    fn index(&self, i: usize) -> &T {
        &self.0[i]
    }
}

#[allow(dead_code)] // production reads go through `get`; this is for tests/ledgers
impl<T> std::ops::IndexMut<usize> for PerHarness<T> {
    fn index_mut(&mut self, i: usize) -> &mut T {
        &mut self.0[i]
    }
}

impl<T: serde::Serialize> PerHarness<T> {
    /// The wire form: a map keyed by harness id, in registry order.
    ///
    /// Used where a payload has to name its keys (the `harness_list` mirror,
    /// the spawn-signature map). A `BTreeMap` rather than the array, because a
    /// positional array on the wire is the exact defect locked decision 8 is
    /// about.
    #[allow(dead_code)] // lands with the spawn-sig map in Phase B
    pub fn as_map(&self) -> BTreeMap<&'static str, &T> {
        self.iter().filter_map(|(h, v)| h.id().map(|i| (i, v))).collect()
    }
}

/// Build a `PerHarness<bool>` from `(id, value)` pairs — **tests only**.
///
/// Naming harnesses is what a test fixture is for (`layering`'s identity scan
/// drops test regions for exactly this reason); what must not happen is a
/// production path spelling one.
#[cfg(test)]
pub fn per_harness_for_test(pairs: &[(&str, bool)]) -> PerHarness<bool> {
    PerHarness::from_fn(|h| {
        h.id()
            .and_then(|id| pairs.iter().find(|(name, _)| *name == id))
            .is_some_and(|(_, on)| *on)
    })
}

impl<T: Default + Copy> Default for PerHarness<T> {
    fn default() -> Self {
        PerHarness::filled(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A role name must be a tool the harness actually serves** (locked
    /// decision 24).
    ///
    /// `tool_for_role` feeds text a model reads ("prefer … over a full Read"),
    /// and its answer is a second spelling of a name that already exists in
    /// `native_tools()`. Two spellings drift: a harness that renamed its read
    /// tool would keep steering the model at the old name, in prose, silently.
    #[test]
    fn every_declared_tool_role_names_a_native_tool() {
        use super::super::plugin::ToolRole;
        for id in all() {
            let Some(plugin) = id.plugin() else { continue };
            for role in [ToolRole::Read, ToolRole::Shell] {
                let Some(name) = plugin.tool_for_role(role) else {
                    continue;
                };
                assert!(
                    plugin.native_tools().iter().any(|t| t.name == name),
                    "{id}: declares {name:?} for {role:?}, which is not one of its native tools \
                     — the guidance would steer the model at a name it does not serve"
                );
            }
        }
    }

    /// **At most one harness takes cImp's unrecognised argv** (locked decision
    /// 26).
    ///
    /// cImp is documented as a drop-in for ONE binary. Two harnesses answering
    /// `true` would forward one CLI's flags into the other's tabs — a launch
    /// failure with no visible cause — and would make `passthrough_harness`
    /// answer `None`, which would silently drop the contract from `--help`.
    #[test]
    fn at_most_one_harness_takes_the_passthrough_argv() {
        let takers: Vec<&str> = HARNESSES
            .iter()
            .filter(|d| d.plugin.accepts_passthrough_argv())
            .map(|d| d.id)
            .collect();
        assert!(
            takers.len() <= 1,
            "cImp can only be a drop-in replacement for one binary; these claim it: {takers:?}"
        );
        assert_eq!(
            passthrough_harness().map(|h| h.token()),
            takers.first().copied(),
            "`passthrough_harness` and the descriptors disagree"
        );
    }

    /// **Two plugins may not claim one subcommand flag, and none may claim a
    /// flag core answers itself** (V40 review finding L-3).
    ///
    /// `subcommand_for` scans the whole of `argv`, takes the FIRST registry
    /// match, and is dispatched in `main.rs` *before* `--offload-mcp`,
    /// `--code-audit-mcp` and the retired-shim tombstones. Plugin ROUTES have
    /// both guards (`no_two_plugins_claim_one_route`,
    /// `no_plugin_route_shadows_a_core_route`); subcommands had neither, so a
    /// second declaration of `--statusline` would be answered by whichever
    /// harness the registry lists first, and a plugin declaring
    /// `--offload-mcp` would take over cImp's own MCP child with nothing
    /// anywhere to say so.
    ///
    /// Core's flags are read out of `main.rs` rather than hand-listed, the way
    /// the route guard reads core's dispatch: a flag added there is covered the
    /// day it is added. Newline-agnostic: CI checks this tree out with CRLF.
    #[test]
    fn no_two_plugins_claim_one_subcommand_and_none_shadows_a_core_flag() {
        let mut seen: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for d in HARNESSES {
            for sub in d.plugin.subcommands() {
                assert!(
                    sub.flag.starts_with("--"),
                    "{}: subcommand flag {:?} is not a `--flag`",
                    d.id,
                    sub.flag
                );
                if let Some(prior) = seen.insert(sub.flag, d.id) {
                    panic!(
                        "`{}` is declared by both `{prior}` and `{}` — `subcommand_for` takes                          the first registry match, so one of them would silently never run",
                        sub.flag, d.id
                    );
                }
            }
        }

        // Core's own flags, scraped from its entry point: the `a == "--x"`
        // comparisons plus the retired-shim tombstone array.
        /// Collect every `"--flag"` literal in `hay` that follows `opener`.
        fn flags_after<'a>(
            hay: &'a str,
            opener: &str,
            out: &mut std::collections::BTreeSet<&'a str>,
        ) {
            let mut rest = hay;
            while let Some(i) = rest.find(opener) {
                rest = &rest[i + opener.len()..];
                // `rest` now starts just after the opening quote.
                if let Some((flag, tail)) = rest.split_once('"') {
                    out.insert(flag);
                    rest = tail;
                }
            }
        }
        let main_src = include_str!("../main.rs");
        let mut core_flags: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        // `if args.iter().any(|a| a == "--offload-mcp")` and friends.
        flags_after(main_src, "a == \"", &mut core_flags);
        // …and the retired-shim tombstones, which are an array of literals.
        let tombstones = main_src
            .split_once("RETIRED_HOOK_FLAGS")
            .expect("the tombstone array is gone — re-point this scan")
            .1;
        let tombstones = &tombstones[..tombstones.find("];").expect("the array closes")];
        flags_after(tombstones, "\"", &mut core_flags);
        core_flags.retain(|f| f.starts_with("--"));
        // Non-vacuity: the two flags a plugin taking over would matter most for
        // are cImp's own MCP children, and both are dispatched AFTER the
        // subcommand loop.
        for must in ["--offload-mcp", "--code-audit-mcp"] {
            assert!(
                core_flags.contains(must),
                "the core flag scan missed `{must}` — it would pass by finding nothing:                  {core_flags:?}"
            );
        }
        assert!(
            !seen.is_empty(),
            "no harness declares a subcommand any more — this guard would pass by iterating              nothing (`--statusline` is the one that exists)"
        );
        for (flag, owner) in &seen {
            assert!(
                !core_flags.contains(flag),
                "`{owner}` declares `{flag}`, which cImp's own entry point answers — the                  subcommand dispatch runs FIRST, so the plugin would take the flag over"
            );
        }
    }

    #[test]
    fn ids_are_unique_and_non_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for d in HARNESSES {
            assert!(!d.id.is_empty(), "the empty id is reserved for HarnessId::ANY");
            assert!(seen.insert(d.id), "duplicate harness id {}", d.id);
            assert!(!d.binaries.is_empty(), "{} declares no binary", d.id);
            assert!(!d.tabs.is_empty(), "{} declares no tab id", d.id);
        }
    }

    #[test]
    fn any_is_not_a_harness() {
        assert_eq!(HarnessId::ANY.id(), None);
        assert_eq!(HarnessId::ANY.token(), "any");
        assert!(HarnessId::ANY.descriptor().is_none());
        assert!(HarnessId::ANY.ordinal().is_none());
        // The neutral token must not resolve to a product, or `--harness-canary
        // any` would drive a CLI nobody named.
        assert!(HarnessId::from_id("any").is_none());
    }

    #[test]
    fn an_unregistered_command_is_not_a_harness() {
        // The six-Claude-fallbacks defect, as an assertion.
        assert!(HarnessId::from_command("foo").is_none());
        assert!(HarnessId::from_command("claude-code.cmd").is_none());
        assert!(HarnessId::from_consumer("codex").is_none());
        assert!(HarnessId::from_tab_id("ai-1234").is_none());
    }

    #[test]
    fn a_command_resolves_by_file_stem_case_insensitively() {
        let claude = HarnessId::from_id("claude").expect("claude is registered");
        assert_eq!(HarnessId::from_command("claude"), Some(claude));
        assert_eq!(HarnessId::from_command("claude.exe"), Some(claude));
        assert_eq!(HarnessId::from_command("/usr/bin/claude"), Some(claude));
        assert_eq!(HarnessId::from_command("C:\\bin\\CLAUDE.EXE"), Some(claude));
    }

    #[test]
    fn every_reserved_tab_id_resolves_to_exactly_one_harness() {
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for tab in canonical_tab_ids() {
            assert!(seen.insert(tab), "tab id {tab} is claimed by two harnesses");
            assert!(HarnessId::from_tab_id(tab).is_some());
        }
    }

    #[test]
    fn the_default_harness_is_registered() {
        // The wire-compat default is a promise about a real build; if it ever
        // stopped naming a registered harness the promise would be unkeepable.
        assert!(DEFAULT_HARNESS.descriptor().is_some());
    }

    #[test]
    fn per_harness_covers_the_registry_and_nothing_else() {
        let p = PerHarness::from_fn(|h| h.id().unwrap_or("?").to_string());
        assert_eq!(p.iter().count(), COUNT);
        for h in all() {
            assert_eq!(p.get(h).map(String::as_str), h.id());
        }
        assert!(p.get(HarnessId::ANY).is_none());
    }
}

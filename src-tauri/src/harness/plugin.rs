//! V40 Phase A — **`HarnessPlugin`**: the L1 interface, and the neutral types
//! it speaks.
//!
//! Locked decision 4. The registry ([`super::registry`]) answers *which*
//! harness; this trait is *what that harness does* — the code half, the part
//! that cannot be a table because OOB transport, env composition and artifact
//! writing are behaviour.
//!
//! # Why a trait, when design D1 says the extension point is the protocol
//!
//! D1 is about **L2**: a third party extends cImp by speaking CHP, never by
//! linking Rust. This trait sits at **L1**, below CHP, and is internal to the
//! tree (D7: nothing loads a plugin from outside). It exists so that the ~11
//! places `tabs/config.rs` used to branch on `command_is(.., "claude")` ask a
//! question instead of making a judgement.
//!
//! # The default is always the harness-neutral answer
//!
//! Every method has a default that does nothing observable — no pre-args, no
//! artifacts, no OOB source, no input profile. That is deliberate and it is the
//! *fail-closed* direction: a harness that has not declared a capability does
//! not get it, rather than inheriting Claude's. `input_profile` is the sharpest
//! case — a harness with no `input.rs` returns `None`, fails the
//! `delegation.worker` gate, and is **not a valid worker** (V39 locked decision
//! 16), which is a visible refusal instead of a task typed into a tab cImp
//! cannot drive.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::offload::toolclass::ToolClass;
use crate::sandbox::GrantRow;
use crate::settings::{AiToolTabConfig, Settings};

use super::reader::OobSpec;

// ── the input profile (V39, moved here by V40 Phase A, amendment 0-a) ───────

/// How a multi-line request is handed to a TUI.
// `Raw` is unconstructed on purpose — see its own doc comment: it exists so a
// future harness can DECLARE that it has no bracketed-paste path, rather than
// be forced to claim support it does not have. A variant that only appears once
// someone needs it is a variant nobody can find.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteMode {
    /// Wrap the text in the bracketed-paste markers (`ESC [ 200 ~` …
    /// `ESC [ 201 ~`). A TUI in bracketed-paste mode treats everything between
    /// them as *one literal insertion*: newlines land in the input buffer as
    /// newlines instead of being read as as many separate Enter presses, which
    /// is the difference between "one request" and "N truncated turns".
    Bracketed,
    /// Write the bytes as they are. Correct only for a TUI that does not enable
    /// bracketed paste — and then only for single-line requests, because every
    /// embedded newline is a submit. No harness uses this today; it exists so a
    /// future one can declare the truth rather than be forced to claim
    /// bracketed support it does not have.
    Raw,
}

/// One harness's answer to "how do I type a turn into this TUI".
///
/// Deliberately data, not a function: the engine composes the write from these
/// four facts in one place, so two harnesses cannot end up with two different
/// *orders* of the same steps. What varies per harness is the values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputProfile {
    /// Paste encoding — see [`PasteMode`].
    pub paste: PasteMode,
    /// The bytes that submit the composed input as a turn. `b"\r"` for both
    /// harnesses today (a TUI reads Enter as CR from a PTY, not LF).
    pub submit: &'static [u8],
    /// How long to wait between the paste and the submit.
    ///
    /// **Not cosmetic.** Both TUIs debounce a paste before they re-render, and
    /// a submit that arrives inside that window is processed against a buffer
    /// the TUI has not finished ingesting — the observable symptom being a turn
    /// that carries only the first line, or a `[Pasted text]` placeholder that
    /// submits anyway. The value is a floor, not a measurement (see the module
    /// docs on what is unverified).
    pub settle_ms: u64,
    /// The largest request this profile will type, in bytes.
    ///
    /// A bound, not a truncation point: the engine **refuses** an oversize task
    /// naming this limit rather than typing a prefix of it. Silently sending
    /// half a request is the one failure mode a worker cannot report — it would
    /// answer the truncated question perfectly.
    pub max_paste_bytes: usize,
}

impl InputProfile {
    /// The bytes that carry `task` into the TUI's input buffer, encoded per
    /// [`Self::paste`]. The task is passed through **verbatim** (locked
    /// decision 2a/10): no header, no marker, nothing a worker model could read
    /// as provenance.
    pub fn paste_bytes(&self, task: &str) -> Vec<u8> {
        match self.paste {
            PasteMode::Bracketed => {
                let mut out =
                    Vec::with_capacity(task.len() + BRACKET_START.len() + BRACKET_END.len());
                out.extend_from_slice(BRACKET_START);
                out.extend_from_slice(task.as_bytes());
                out.extend_from_slice(BRACKET_END);
                out
            }
            PasteMode::Raw => task.as_bytes().to_vec(),
        }
    }

    /// Whether `task` fits this profile's paste bound. `false` ⇒ the engine
    /// refuses, naming the limit.
    pub fn fits(&self, task: &str) -> bool {
        task.len() <= self.max_paste_bytes
    }
}

/// `ESC [ 200 ~` — the start of a bracketed paste.
const BRACKET_START: &[u8] = b"\x1b[200~";
/// `ESC [ 201 ~` — the end of one.
const BRACKET_END: &[u8] = b"\x1b[201~";

// ── sandbox grants ──────────────────────────────────────────────────────────

/// What a plugin is handed when it declares its sandbox grant rows.
///
/// The *environment reader* rather than the environment, because the grant
/// tables must be testable against a synthetic environment — which is exactly
/// how `sandbox::tabs`' own tests drive them today, and the reason
/// `grant_rows_with` took a closure before this trait existed.
pub struct GrantCtx<'a> {
    /// The user's home directory, already resolved.
    pub home: &'a Path,
    /// Read one environment variable.
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
}

impl GrantCtx<'_> {
    /// XDG spelling first where the harness honors it, then the default under
    /// [`Self::home`] — OpenCode reads `XDG_CONFIG_HOME`/`XDG_DATA_HOME` on
    /// Windows too, and a user who relocated them would otherwise get a
    /// boundary around the wrong directories with no clue why.
    pub fn xdg(&self, var: &str, default: &[&str]) -> PathBuf {
        (self.env)(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| default.iter().fold(self.home.to_path_buf(), |p, seg| p.join(seg)))
    }
}

// ── probe output ────────────────────────────────────────────────────────────

/// What one harness's probe run produced: the capability rows, the payloads
/// worth keeping in the capture corpus, and the CLI version the run observed.
#[derive(Default)]
pub struct ProbeOutput {
    pub results: Vec<super::probe::ProbeResult>,
    pub observed: Vec<super::capture::Observed>,
    /// The version this run saw. Empty ⇒ the runner falls back to
    /// [`HarnessPlugin::recorded_version`].
    pub version: String,
}

// ── harness-native tools (locked decision 16) ───────────────────────────────

/// Which argument of a classified tool carries the recorded target.
///
/// Lives here rather than in `graph/memory.rs` because it is part of what the
/// trait speaks: a harness declares the memory shape of its own tools, and L1
/// may not import an L4 capability (`layering::harness_modules_do_not_import_capabilities`).
/// `graph` re-exports it, so every existing consumer keeps its spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemArg {
    /// `file_path` — a concrete file (recorded as `path`).
    Path,
    /// `pattern`/`path` of a search (recorded as `path`, best-effort).
    Pattern,
    /// `command` of a shell call (recorded into `detail`, no path).
    Command,
}

/// One tool a HARNESS serves itself — a name cImp never routes and therefore
/// can never block (locked decision 3's honest limit).
///
/// The three columns are the three questions core asks about such a name, each
/// of which used to be answered by a different table with a different default:
/// [`Self::class`] (what would gate it), [`Self::mutates_fs`] (whether a
/// pre-tool checkpoint fires) and [`Self::memory_kind`] (what memory event it
/// is recorded as). One row per name, in one reviewed place per harness, is
/// what stops the three from disagreeing.
///
/// Read through [`crate::harness::native`], never directly, so the fail-closed
/// rule for an unidentified source is applied in exactly one place.
pub struct NativeTool {
    /// The id as the harness itself spells it. Vocabularies do not cross:
    /// `Edit` is Claude's and `edit` is OpenCode's.
    pub name: &'static str,
    /// The containment class, or `None` when cImp makes **no gating claim**
    /// about this name.
    ///
    /// `None` is not "unclassified": it is the allowlist-only posture a
    /// harness registry needs, where an unlisted (or unclassed) name is
    /// UNGATED because the set is closed and published and most of its members
    /// are neither external nor local-capability. That is the opposite of
    /// [`crate::offload::toolclass::classify`]'s unknown-⇒-EXTERNAL, which is
    /// right for cImp's own routed vocabulary and wrong here.
    pub class: Option<ToolClass>,
    /// Whether calling this tool can change files on disk — V33 Phase F's
    /// pre-mutation checkpoint trigger.
    pub mutates_fs: bool,
    /// The V10 memory event this call is recorded as: the `kind` string and
    /// which argument carries its target. `None` for a tool that is not
    /// recorded at all (orchestration, bookkeeping, cImp's own proxied tools —
    /// already captured by the activity ring).
    pub memory_kind: Option<(&'static str, MemArg)>,
}

// ── declared settings (locked decision 6) ───────────────────────────────────

/// What a declared `ext` field HOLDS — the parse-boundary check and the widget
/// the generic Settings form renders, in one declaration.
///
/// Deliberately a small closed set. A harness setting that needed a bespoke
/// widget would be a harness-shaped UI in core again, which is the thing this
/// milestone is removing; [`Self::Json`] is the escape hatch and it renders
/// nothing (see its docs).
// `Int`, `Path` and `Enum` are unconstructed today, and deliberately declared
// anyway — the same reasoning as `PasteMode::Raw`. This is the vocabulary a
// harness's settings are stated IN, and a form that can only render checkboxes
// and text boxes is one a harness author works around rather than declares
// against. A kind that only appears once someone needs it is a kind nobody can
// find.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// A checkbox.
    Bool,
    /// A whole number.
    Int,
    /// A free-text line.
    Text,
    /// A filesystem path. Stored and validated as text; the form offers a
    /// picker.
    Path,
    /// One of a fixed list of tokens.
    Enum(&'static [&'static str]),
    /// An opaque value **cImp itself writes** — a derived block the user never
    /// types (OpenCode's `local-llama` provider is the case). The generic form
    /// does not render it; validation only checks that it is `null` or an
    /// object, because its shape is the plugin's business and core does not
    /// know it.
    Json,
}

impl SettingKind {
    /// Whether `v` is an acceptable value for this kind.
    ///
    /// **The parse boundary, not a suggestion** (global principle 4). A
    /// declared schema is a claim about a file the user can hand-edit; without
    /// a post-hoc check the claim holds only as long as nobody edits the file,
    /// and a `"statusline": "yes"` would reach the launch path as a
    /// `serde_json::Value::String` that every reader silently answers `false`
    /// for.
    pub fn accepts(self, v: &serde_json::Value) -> bool {
        match self {
            SettingKind::Bool => v.is_boolean(),
            SettingKind::Int => v.is_i64() || v.is_u64(),
            SettingKind::Text | SettingKind::Path => v.is_string(),
            SettingKind::Enum(options) => {
                v.as_str().is_some_and(|s| options.contains(&s))
            }
            SettingKind::Json => v.is_null() || v.is_object(),
        }
    }
}

/// A declared default, as data — so [`SettingField`] can be a `const` table.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingDefault {
    Bool(bool),
    Int(i64),
    Text(&'static str),
    /// No value: an empty [`SettingKind::Json`] block.
    Null,
}

impl SettingDefault {
    /// This default as the JSON that lands in the stored `ext` map.
    pub fn to_json(self) -> serde_json::Value {
        match self {
            SettingDefault::Bool(b) => serde_json::Value::Bool(b),
            SettingDefault::Int(i) => serde_json::Value::Number(i.into()),
            SettingDefault::Text(s) => serde_json::Value::String(s.to_string()),
            SettingDefault::Null => serde_json::Value::Null,
        }
    }
}

/// One field in a harness's own settings namespace
/// (`Settings.harness[<id>].ext`).
///
/// Locked decision 6: a setting only ONE harness reads is that harness's, and
/// core stores it opaquely. Core never names a key — it validates the stored
/// object against this table at the parse boundary, renders the section from
/// it, folds the [`Self::spawn_baked`] ones into the spawn signature, and
/// otherwise hands the value straight back to the plugin that declared it.
///
/// The consequence worth stating: a harness that adds a setting adds a row
/// here and nothing else. No `Settings` field, no migration (an absent key
/// reads its default), no restart-hint wiring, no Settings-window markup.
pub struct SettingField {
    /// The key inside `ext`. Unique per harness; dotted grouping (`local.
    /// base_url`) is a convention the form reads as a section, not a nested
    /// object.
    pub key: &'static str,
    /// What it holds — see [`SettingKind`].
    pub kind: SettingKind,
    /// The form's label.
    pub label: &'static str,
    /// One sentence under the control. May be empty.
    pub hint: &'static str,
    /// The value an absent key reads as.
    ///
    /// **This is what makes a later harness need no migration** (locked
    /// decision 5): the 35 -> 36 step copies what EXISTS, and everything that
    /// does not exist resolves here instead of being backfilled.
    pub default: SettingDefault,
    /// Whether this value reaches the harness only at SPAWN.
    ///
    /// `true` puts it in that harness's `spawn_inject_sig` slot automatically,
    /// which closes the "spawn-baked setting with no signature entry" class
    /// that V38 M-3 and V32 F-27 both landed in: the flag and the signature are
    /// now one declaration instead of two lists that could disagree.
    pub spawn_baked: bool,
    /// Whether the value is a credential.
    ///
    /// Redacted by [`crate::settings::HarnessSettings`]'s `Debug` — the same
    /// defense-in-depth the hand-rolled `ClaudeLocalSettings::fmt` carried
    /// before its three fields became `ext` rows, kept so an accidental
    /// `?settings` log line cannot leak an auth token to the rolling log.
    pub secret: bool,
}

/// One injection-hierarchy feature a harness declares itself subject to, with
/// the `ext` key holding its app-wide L2.
///
/// Both halves are the plugin's, and that is the point: `Feature` is core's
/// vocabulary, the ext key is the harness's, and the JOIN between them is
/// declared by the only party that knows both. Core resolves a scoped feature's
/// L2 by asking every plugin for its row — it never spells the key, and it
/// keeps no list of which features are scoped.
pub struct ScopedFeature {
    /// The core feature this harness delivers.
    pub feature: crate::settings::injection::Feature,
    /// This harness's `ext` key holding that feature's app-wide L2. Must be a
    /// [`SettingKind::Bool`] row in [`HarnessPlugin::settings_schema`] —
    /// asserted by `layering::every_registry_entry_is_fully_wired`.
    pub ext_key: &'static str,
}

// ── canaries (locked decision 17) ───────────────────────────────────────────

/// One **L1 canary**: a committed fixture, and the assertion that the reader
/// behind one capability still produces substantive output from it.
///
/// Declared by the plugin rather than matched on in core (V40 Phase A, locked
/// decision 17). The three fields are the three things the neutral runner needs
/// and cannot know: *which* capability this proves, *what* bytes it proves it
/// against, and *how*.
///
/// **A canary id IS a capability id** — never a third namespace. That is what
/// lets `canary::tests::embedded_canaries_are_exactly_the_declared_ones`
/// set-compare the declared canaries against the registry's `canary` column in
/// both directions, mechanically, instead of against a hand-maintained list.
pub struct Canary {
    /// The [`crate::harness::contract::Capability::id`] this canary proves.
    pub id: &'static str,
    /// The fixture, `include_str!`-embedded so the check runs from a RELEASE
    /// binary: a runtime canary must never be able to degrade to "fixture not
    /// found ⇒ skipped". Handed to [`Self::run`] by the runner, so the bytes the
    /// corpus walker checks and the bytes the shipped canary reads are the same
    /// value rather than two consts that could part company.
    pub fixture: &'static str,
    /// That fixture's `<harness>/<version>/<name>` path under
    /// `src-tauri/fixtures/harness/`.
    ///
    /// Carried beside the bytes so the corpus walker can check the embedded
    /// copy against the committed file without a second, hand-kept pair list —
    /// which is exactly what the old list was, and it had grown a hole.
    // Read by `canary::tests::the_embedded_fixtures_are_the_committed_files` —
    // the corpus check IS its consumer, the same shape `Capability`'s
    // documentation-only columns carry.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fixture_path: &'static str,
    /// The assertion. Returns `Err(detail)` on drift; `detail` is what the
    /// *Harness health* panel and `cimp --harness-canary` print verbatim.
    ///
    /// Takes the fixture rather than closing over it, which is what lets the
    /// **negative** twin drive this exact function against a `_synthetic/` drift
    /// model: "the canary fires" is then proven about the production path, not
    /// about a copy of it.
    ///
    /// A plain `fn` pointer rather than a closure type, so the table can be a
    /// `const` and the runner's dispatch is a lookup rather than a `match`. An
    /// `async` reader is wrapped by its own entry (see OpenCode's).
    pub run: fn(&str) -> Result<(), String>,
}

// ── the trait ───────────────────────────────────────────────────────────────

/// Everything core asks *this harness* to do.
///
/// Implemented once per `harness/<id>/`, held by the descriptor as
/// `&'static dyn HarnessPlugin`. Every method's default is the neutral answer;
/// see the module docs for why that direction is the safe one.
pub trait HarnessPlugin: Sync + Send {
    // ── delegation (V39) ────────────────────────────────────────────────────

    /// How a turn is typed into this harness's TUI, or `None` when it declares
    /// none — in which case this harness is not a delegation worker at all.
    ///
    /// `Option` with a `None` default is load-bearing: it is what keeps "a
    /// harness without an `input.rs` is not a valid worker" a fail-closed
    /// property rather than a panic or, worse, another harness's paste rules.
    fn input_profile(&self) -> Option<InputProfile> {
        None
    }

    // ── tab launch ──────────────────────────────────────────────────────────

    /// Arguments this harness needs **before** the user's own — the
    /// `--append-system-prompt` addendum, the `--settings` overlay, the
    /// `--mcp-config` server set.
    fn pre_args(
        &self,
        _cfg: &AiToolTabConfig,
        _settings: &Settings,
        _tab: &str,
        _endpoint: Option<&crate::offload::loopback::Discovery>,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Why this harness refuses one of the tab's stored arguments, if it does.
    ///
    /// The refusal is a *correction with a log line*, not a launch failure: a
    /// flag the harness rejects at startup would otherwise hand the user an
    /// opaque usage error from a CLI they did not type into.
    fn arg_is_rejected(&self, _arg: &str) -> Option<&'static str> {
        None
    }

    /// Whether cImp's own invocation argv (`cimp --resume <id>`) is forwarded
    /// into this harness's tabs.
    ///
    /// True only where cImp is documented as a drop-in replacement for the
    /// harness's own binary. A harness that selects its model and session
    /// through config rather than flags gets `false`, because forwarding
    /// another CLI's flags into it is how a tab fails to launch.
    fn accepts_passthrough_argv(&self) -> bool {
        false
    }

    /// This harness's additions to a tab's child environment.
    ///
    /// Writes into `env` in place, because the neutral composer owns the order:
    /// synthesized values first, the user's per-tab `env` last, so a per-tab
    /// override always wins.
    fn compose_env(
        &self,
        _cfg: &AiToolTabConfig,
        _settings: &Settings,
        _tab: &str,
        _endpoint: Option<&crate::offload::loopback::Discovery>,
        _env: &mut HashMap<String, String>,
    ) {
    }

    /// Files this harness needs on disk before its tab launches (a managed
    /// instructions file, a generated plugin).
    ///
    /// Kept off the pure `compose_env` path so the config builders stay
    /// test-safe: this is the one launch step that touches the filesystem.
    fn write_artifacts(
        &self,
        _cfg: &AiToolTabConfig,
        _settings: &Settings,
        _tab: &str,
        _working_dir: &Path,
    ) {
    }

    /// The out-of-band source for this tab, if any — and the flags that make it
    /// reachable, appended to `extra_args`.
    ///
    /// `env` is the environment this child will be spawned with, and it is read
    /// (never written): a credential the tap must present has to be the one the
    /// child will actually read.
    fn resolve_oob(
        &self,
        _cfg: &AiToolTabConfig,
        _working_dir: &Path,
        _extra_args: &mut Vec<String>,
        _env: &HashMap<String, String>,
    ) -> Option<OobSpec> {
        None
    }

    /// Record this harness's installed version at tab spawn, if it has a way to
    /// observe one there. Best-effort and non-blocking in every direction — a
    /// version note must never delay a tab launch.
    fn note_version(&self, _command: &str) {}

    /// Everything Settings-derived that reaches this harness's tabs **only at
    /// spawn**, as one comparable value.
    ///
    /// Compared across a Settings save to decide whether a "restart the AI tab"
    /// hint is due. Coarse by design: any difference means a fresh tab would be
    /// launched differently from the one still running.
    fn spawn_sig(&self, _settings: &Settings) -> serde_json::Value {
        serde_json::Value::Null
    }

    // ── declared settings (locked decision 6) ───────────────────────────────

    /// This harness's OWN settings — the fields core stores in
    /// `Settings.harness[<id>].ext` and never names.
    ///
    /// V40 Phase B. What used to be here instead was `recorded_version` /
    /// `auto_verify_record` / `last_verified`: three plugin methods whose only
    /// job was to read `hv.claude_last_seen`-shaped field PAIRS out of core on
    /// core's behalf. The pairs are a `BTreeMap<HarnessId, HarnessSettings>`
    /// now, so core reads the row directly and the methods are gone — the
    /// interface should carry what core cannot know, and "which of my two
    /// fields holds my version" was never that.
    ///
    /// What core genuinely cannot know is the rest: whether this harness has a
    /// status line, a local-provider block, a native-tool gate. Those are the
    /// rows. An empty table is an ordinary answer — such a harness gets an
    /// empty Settings section and no ext keys, with no work anywhere.
    fn settings_schema(&self) -> &'static [SettingField] {
        &[]
    }

    /// Injection-hierarchy features this harness — and only harnesses that
    /// declare them — is subject to.
    ///
    /// The mechanism behind a scoped feature lives inside the harness's own
    /// config or plugin (OpenCode's `tool.execute.before` gate is the case), so
    /// asking a Claude tab about it produced a restart hint for a control that
    /// could not reach it. Core derives "is this feature scoped at all?" from
    /// this list across the registry rather than holding one of its own, so a
    /// feature nobody declares stays app-wide and a feature two harnesses
    /// declare reaches both.
    fn scoped_features(&self) -> &'static [ScopedFeature] {
        &[]
    }

    // ── native tool vocabulary (locked decision 16) ─────────────────────────

    /// The tools this harness serves ITSELF — the names cImp never routes.
    ///
    /// Empty is the fail-closed answer and it is a loud one: with no rows,
    /// every one of this harness's tool calls is treated as mutating and none
    /// is recorded as a memory event, which
    /// `native::tests::every_registered_harness_declares_its_natives` refuses
    /// to let a registered harness ship with.
    fn native_tools(&self) -> &'static [NativeTool] {
        &[]
    }

    // ── the capability registry (locked decision 17) ────────────────────────

    /// This harness's own rows in the capability registry.
    ///
    /// Rows whose CONTRACT is a sentence about this product — "Claude Code's TUI
    /// accepts a bracketed paste as one literal insertion" — rather than about a
    /// tab. `contract::capabilities()` chains them after the neutral ones, so
    /// every consumer (the gate, the probe, the health panel, the drift advisor,
    /// the literal scan) sees one registry and none of them holds a per-harness
    /// list.
    ///
    /// A harness that declares an [`InputProfile`] must declare the row that
    /// states what that profile depends on — asserted by
    /// `layering::every_registry_entry_is_fully_wired`, because a profile with
    /// no row is a Tier-D behaviour nothing records, nothing degrades on and
    /// nobody can mark verified.
    fn capabilities(&self) -> &'static [crate::harness::contract::Capability] {
        &[]
    }

    // ── canaries (locked decision 17) ───────────────────────────────────────

    /// This harness's L1 canaries — the fixture-backed substantiveness checks
    /// that run every `cargo test` AND inside the shipped binary whenever this
    /// harness's version changes.
    ///
    /// Empty is an ordinary answer for a harness cImp only pushes to; it means
    /// the registry declares no `canary: Some(..)` row for it, and the
    /// set-comparison in `canary.rs` holds either way.
    fn canaries(&self) -> &'static [Canary] {
        &[]
    }

    // ── probes (locked decision 17) ─────────────────────────────────────────

    /// Drive this harness's L2 probes against the **installed** CLI.
    ///
    /// Returns the rows, whatever payloads were observed (for the capture
    /// corpus) and the version the run saw — empty when the CLI produced none,
    /// in which case the runner falls back to [`Self::recorded_version`].
    fn probe(&self) -> ProbeOutput {
        ProbeOutput::default()
    }

    /// Whether this harness's probes share **one expensive child process**, in
    /// which case the runner drives it before harnesses whose probes are
    /// independent.
    ///
    /// `opencode serve` is the case: one child answers every OpenCode probe, and
    /// starting it while another harness's probes run would hold it open for no
    /// reason. Declared rather than hard-coded as an order, so the reason
    /// travels with the harness it is true of.
    fn probes_share_one_child(&self) -> bool {
        false
    }

    /// Capability ids this harness declares **permanently unprobed**, each with
    /// the reason.
    ///
    /// A SEPARATE list from what [`Self::probe`] answers, on purpose: "no probe
    /// can settle this" is a claim that needs writing down, and a probe that
    /// silently stopped emitting a row must not be mistaken for one.
    fn declared_unprobed(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    // ── sandbox ─────────────────────────────────────────────────────────────

    /// Where this harness keeps its own state, as grant rows with a reason each.
    ///
    /// **Read an implementation as a security review, not as configuration.**
    /// Every row widens what a compromised agent can read. A harness that
    /// declares none is not sandboxed by a table nobody wrote — the caller
    /// treats an empty list as "no harness rows", and `every_registry_entry_is_
    /// fully_wired` refuses to let a registered harness ship with one.
    fn sandbox_grants(&self, _ctx: &GrantCtx) -> Vec<GrantRow> {
        Vec::new()
    }
}

// ── neutral launch helpers ──────────────────────────────────────────────────

/// Whether the `cimp-code-audit` MCP server is advertised to `harness`'s tabs.
///
/// V40 Phase B. It replaced `tabs::config::advertises_audit_to_claude` /
/// `_to_opencode` — two identical bodies differing only in which half of the
/// `code_audit.expose_*` field pair they read, called from inside
/// `harness/<id>/`, which made the pair an upward edge as well as a pair. One
/// body over `Settings::harness[<id>].expose_code_audit` is both.
///
/// ANDed with the master switch here, once, so no caller can advertise the
/// server for a harness while Code Audit itself is off.
pub fn audit_advertised(s: &Settings, harness: crate::harness::HarnessId) -> bool {
    s.code_audit.enabled && s.harness_settings(harness).expose_code_audit
}

/// Whether the capability matrix currently BLOCKS the read advisor's
/// `PreToolUse` deny (V35 Phase E).
///
/// One named helper for the call sites that install the hook and the ones that
/// put it in a spawn signature — the two must move together — so the capability
/// id is spelled once. Lives here rather than in `tabs::config` since V40 Phase
/// B: its readers are all inside `harness/`, and asking core to ask
/// `harness::contract` on their behalf was a detour with an upward edge in it.
pub fn read_advisor_gate_blocked(s: &Settings) -> bool {
    crate::harness::contract::gate(crate::harness::contract::CAP_PRETOOLUSE_DENY, s).blocked
}

/// The capability-guidance gates **every** harness's spawn signature carries.
///
/// Neutral: the addendum is composed from cImp's own features (offload, the
/// code graph, semantic search, pinned facts) and reaches both harnesses — one
/// through `--append-system-prompt`, the other through a managed instructions
/// file. Only the *transport* is per harness, so only the transport is in a
/// plugin.
///
/// V32 Phase D's injection-hygiene paragraph has no entry of its own on
/// purpose, and V37 Phase F changed WHICH entry covers it. Its gate used to be
/// `advertises_offload_to_{claude,opencode}`; it is now `consumer_hygiene_for`
/// alone, whose L1/L2/L3 cells all ride `injection::spawn_sig` — carried by the
/// `"injection"` entry each plugin adds. A future addendum with an independent
/// gate does need its own slot here.
pub fn guidance_gates(s: &Settings) -> serde_json::Value {
    serde_json::json!([
        s.offload.enabled && s.offload.inject_guidance,
        s.graph.enabled,
        s.graph.enabled && s.graph.semantic_search,
        s.graph.enabled && s.graph.promote_pinned_facts,
    ])
}

/// The tab-sandbox gates **every** harness's spawn signature carries.
///
/// V33 Phase B: the tab sandbox is baked at spawn in the most literal sense —
/// an OS boundary is put around the process at `CreateProcessW` time and cannot
/// be added to or removed from a running one. Without a slot, ticking "Also
/// sandbox AI tabs" would leave every open tab unconfined with no restart hint,
/// and the user would reasonably believe the switch took.
///
/// The EFFECTIVE value: `tabs` alone changes no spawn while the master switch is
/// off. `extra_grant_dirs` rides along because the grants are applied during
/// preparation. `allow_network` is deliberately ABSENT: it does not govern tabs
/// (a sandboxed tab always has egress, decision B3), and nagging every tab to
/// restart for a knob that cannot reach them is how a restart hint stops being
/// read.
pub fn sandbox_gates(s: &Settings) -> serde_json::Value {
    serde_json::json!([
        s.sandbox.enabled && s.sandbox.tabs,
        s.sandbox.extra_grant_dirs,
    ])
}

/// Reserve a free loopback TCP port by binding `127.0.0.1:0` and reading the
/// OS-assigned port, then releasing it.
///
/// There is a small window between release and the child re-binding it, but on
/// loopback at launch this is reliable in practice; a collision just means the
/// event tap fails to connect and the tab has no automatic TTS (it still works
/// otherwise). Neutral machinery — the *fact* that a harness hosts a server on
/// a port cImp picks is the plugin's, the reservation is not.
pub fn alloc_loopback_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bracketed_paste_wraps_the_task_verbatim() {
        let p = InputProfile {
            paste: PasteMode::Bracketed,
            submit: b"\r",
            settle_ms: 10,
            max_paste_bytes: 100,
        };
        let bytes = p.paste_bytes("line one\nline two");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\u{1b}[200~line one\nline two\u{1b}[201~",
            "the task must arrive between the markers, unchanged — no header, no marker of ours"
        );
    }

    /// **The submit is never inside the paste.** A CR between the markers is
    /// literal text; the turn is submitted by the separate `submit` write after
    /// the settle. If these two were ever composed into one buffer the settle
    /// would stop existing.
    #[test]
    fn the_paste_carries_no_submit_key() {
        let p = InputProfile {
            paste: PasteMode::Bracketed,
            submit: b"\r",
            settle_ms: 10,
            max_paste_bytes: 100,
        };
        let bytes = p.paste_bytes("hello");
        assert!(!bytes.ends_with(b"\r"));
        assert_eq!(p.submit, b"\r");
    }

    #[test]
    fn raw_mode_passes_the_bytes_through() {
        let p = InputProfile {
            paste: PasteMode::Raw,
            submit: b"\r",
            settle_ms: 0,
            max_paste_bytes: 100,
        };
        assert_eq!(p.paste_bytes("hi"), b"hi".to_vec());
    }

    /// The bound refuses rather than truncates — a half-typed request is the
    /// one failure a worker cannot report.
    #[test]
    fn the_paste_bound_is_a_refusal_not_a_truncation() {
        let p = InputProfile {
            paste: PasteMode::Bracketed,
            submit: b"\r",
            settle_ms: 0,
            max_paste_bytes: 4,
        };
        assert!(p.fits("abcd"));
        assert!(!p.fits("abcde"));
        assert_eq!(
            p.paste_bytes("abcde").len(),
            5 + BRACKET_START.len() + BRACKET_END.len(),
            "encoding does not truncate; `fits` is what the engine asks first"
        );
    }

    /// The default trait body is the fail-closed one: a harness that declares
    /// nothing gets nothing, and in particular is not a delegation worker.
    #[test]
    fn the_default_plugin_declares_nothing() {
        struct Bare;
        impl HarnessPlugin for Bare {}
        let bare = Bare;
        assert!(bare.input_profile().is_none());
        assert!(!bare.accepts_passthrough_argv());
        assert!(bare.arg_is_rejected("--mini").is_none());
        assert_eq!(bare.spawn_sig(&Settings::default()), serde_json::Value::Null);
    }
}

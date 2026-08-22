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

/// A role core has to NAME in text it sends the model — the one thing about a
/// harness's tool vocabulary that a *neutral* sentence cannot avoid mentioning.
///
/// Locked decision 24. `GRAPH_GUIDANCE` says "prefer `graph_outline` →
/// `graph_snippet` over a full **Read**" and "prefer a configured test check
/// over running the test command in **Bash**": both sentences are about cImp's
/// own tools, and both have to name the harness tool they are steering away
/// from. Naming it from a `match` in core is how that blob came to tell every
/// OpenCode session to prefer two tools it does not have.
///
/// Deliberately a tiny closed set: a role exists here because a *neutral*
/// string needs the name, not because the tool is important.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRole {
    /// Read one file's contents (Claude's `Read`, OpenCode's `read`).
    Read,
    /// Run a shell command (Claude's `Bash`, OpenCode's `bash`).
    Shell,
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

// ── hook ingress (locked decisions 15 and 22) ───────────────────────────────

/// What a plugin-owned loopback route answers, **as bytes core does not read**.
///
/// Locked decision 22. Claude's hook routes answer hook-output JSON
/// (`{"continue":true}`, `hookSpecificOutput.additionalContext`,
/// `permissionDecision: "deny"`); a harness whose ingress is an ordinary plugin
/// answers `{"ok":true}`. Core must not know either shape — its whole job here
/// is *status + body*, which is the same job it does for every neutral route.
///
/// The status is carried rather than assumed because "fail-open in HTTP terms"
/// is a per-harness decision: Claude's family answers `200` on every path
/// including refusals, since a non-2xx is a *non-blocking error* the harness
/// logs and there is nothing to log about a hook that had nothing to say.
pub struct HookReply {
    /// The HTTP status core writes.
    pub status: u16,
    /// The body core serializes, verbatim and unread.
    pub body: serde_json::Value,
}

impl HookReply {
    /// A `200` carrying `body`.
    pub fn ok(body: serde_json::Value) -> Self {
        Self { status: 200, body }
    }
}

/// The future one [`Route`] handler returns.
///
/// Boxed because the table is a `const` of `fn` pointers: an `async fn`'s
/// opaque future type cannot be named in one, and the alternative — a `match`
/// in core — is the thing decision 15 deletes.
pub type RouteFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::error::AppResult<HookReply>> + Send + 'a>,
>;

/// One plugin-owned loopback route.
///
/// `method` and `path` are matched by core **after** every CHP-neutral arm, so
/// a plugin can never shadow `/session/hello`, `/mcp/*` or the audit and push
/// routes. Core keeps no harness path literal; the strings live beside the
/// handler that answers them.
pub struct Route {
    /// The HTTP method, e.g. `"POST"`.
    pub method: &'static str,
    /// The absolute path, query string excluded — core strips it before
    /// matching, exactly as it does for its own arms.
    pub path: &'static str,
    /// The handler. Takes the app and the parsed request; answers a
    /// [`HookReply`] core writes without inspecting.
    pub handler: RouteHandler,
}

/// A [`Route`]'s handler: a plain `fn` pointer over a boxed future.
pub type RouteHandler =
    for<'a> fn(&'a tauri::AppHandle, &'a crate::offload::loopback::Request) -> RouteFuture<'a>;

/// The identity a request carries **outside its body**.
///
/// Locked decision 22. A Claude hook's body is the harness's own payload with no
/// room for a CHP envelope, so its `(agent, tab, chp)` triple arrives in
/// `X-CIMP-*` headers instead. That used to be a `claude_hook::is_hook_route`
/// special-case inside core's `note_chp`; it is now a question core asks every
/// registered plugin, and a harness that puts its identity anywhere else answers
/// for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestIdentity {
    /// The CHP version the caller speaks.
    pub chp: u32,
    /// The harness discriminator as sent — bounded and validated by the caller,
    /// exactly like the body field it replaces.
    pub agent: String,
    /// The cImp tab the request claims.
    pub tab: String,
}

// ── permission edges (locked decision 21) ───────────────────────────────────

/// **Which edge of `awaiting_permission` a harness reported** — the neutral half
/// of prompt detection.
///
/// Locked decision 21. The *classification* is the harness's: which of its
/// notification types means "a prompt is on screen", which of its footers is the
/// approval box, what its transcript path is called. What reaches core is this —
/// two states and the tab they belong to — and it is the same pair the TUI-regex
/// detector produces, which is why a hook edge and a scrape edge collapse to one
/// signal at the state manager instead of being two features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionEdge {
    /// A permission prompt is now on screen ⇒ `PermissionPromptDetected`.
    Detected,
    /// The pending call was resolved ⇒ `PermissionPromptResolved`.
    Resolved,
}

// ── activity (locked decision 18) ───────────────────────────────────────────

/// **How core learns that this harness is busy.**
///
/// Locked decision 18. Two answers, and the difference between them is not a
/// preference — it is whether the harness *tells* cImp (an event stream, a
/// push) or whether cImp has to *infer* it from the terminal it is painting
/// into. Core owns the inference machinery either way; what a plugin declares
/// is which of the two applies and, for the inferring case, the timings the
/// inference is sized against.
///
/// This replaces `pty::manager`'s `oob_drives_activity = matches!(spec.oob,
/// Some(OobSpec::OpenCodeEvent { .. }))` — core deciding whether to model a
/// terminal by testing for **one specific harness's** transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivitySource {
    /// The harness reports its own turn boundaries — an event stream, a CHP
    /// push, a reader that emits `StateSignal::HarnessOutputStarted`/`Stopped`
    /// directly. Core runs **no** TUI heuristic for such a tab: a fullscreen
    /// startup repaint would otherwise fake a whole turn.
    OutOfBand,
    /// Nothing reports turn boundaries, so core infers them from the TUI —
    /// a busy-footer marker (authoritative while on screen) arbitrated against
    /// a byte-burst fallback, with the timings below.
    TuiMarkers(ActivityTuning),
}

/// The timings core's TUI activity arbitration is sized against, **for one
/// harness**.
///
/// Every value here is a measurement of somebody else's terminal — how often
/// its spinner repaints, how long its footer blinks out between sub-agent
/// batches, how long a thinking pause can run with no bytes. They were five
/// `CLAUDE_*` constants in `pty/tasks.rs` and `state/manager.rs`, which meant
/// core's avatar was a model of Claude Code's screen and any other harness got
/// it by accident.
///
/// **A `const` table, pinned by a golden test** (`claude::plugin::tests::the_activity_tuning_is_the_pre_v40_constants`):
/// these numbers were tuned against observed behaviour over several
/// milestones, and a refactor that rounds one of them is a regression nobody
/// would see until an avatar flickered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityTuning {
    /// How long a byte burst must be sustained before the timer alone counts
    /// the child as generating — the fallback for a response that never paints
    /// the marker.
    pub burst_min: std::time::Duration,
    /// Quiet interval that closes a burst once the marker is gone. The marker,
    /// not this timer, decides Idle while the harness is working.
    pub quiet: std::time::Duration,
    /// How long the busy marker must be *continuously absent* before a
    /// marker-driven session may settle to Idle. Bridges a footer that blinks
    /// out between sub-agent batches.
    pub marker_grace: std::time::Duration,
    /// Safety valve: the marker is still matched but the byte stream has been
    /// silent this long, so treat it as a ghost and release.
    pub working_stale: std::time::Duration,
    /// Backstop for a wedged sub-agent count: a tab sitting in Thinking with
    /// sub-agents nominally active and the parent producing NO output for this
    /// long is released to Idle. Must be longer than [`Self::working_stale`],
    /// so the marker path always concludes first — asserted by
    /// `harness::plugin::tests::every_stall_backstop_outlasts_its_marker_path`.
    pub subagents_stall: std::time::Duration,
}

// ── usage, quota and context (locked decision 19) ───────────────────────────

/// One quota window a harness can report — **declared**, so core never spells
/// `five_hour` / `seven_day` and a harness with three windows (or none) needs
/// no core change.
///
/// The pre-V40 shape was two named fields on a core struct, which is why the
/// widget, the IPC mirror and the push file all carried one vendor's
/// subscription plan in their vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaWindowSpec {
    /// Stable id — the key a reading is matched to, and what a UI keys a row
    /// on. Never rendered.
    pub id: &'static str,
    /// The row label, e.g. `"current session"`.
    pub label: &'static str,
    /// The window's duration as the harness names it, e.g. `"(5h)"`. Its own
    /// column so the labels line up across rows.
    pub short: &'static str,
    /// One sentence for the row's tooltip.
    pub description: &'static str,
}

/// One billing category a harness reports tokens under.
///
/// input / cache_write / cache_read / output is *Claude's* set. A harness that
/// does not distinguish cache tokens declares fewer, and a category it does
/// not declare is **absent** from a reading — never present as zero (locked
/// decision 19; global principle 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenKindSpec {
    /// Stable id, e.g. `"cache_read"`.
    pub id: &'static str,
    /// The label a UI renders for it.
    pub label: &'static str,
}

/// **One turn's tokens, by declared category** — and the type whose whole
/// purpose is that a category nobody reported has **no entry**.
///
/// It replaces four `Option<u64>` fields named after one vendor's billing
/// (`cache_read_input_tokens` and friends). A map rather than a struct because
/// the set of categories is the harness's declaration
/// ([`TokenKindSpec`]) and not core's vocabulary; a newtype rather than a bare
/// map because the absence rule is the contract, and a bare map invites a
/// `.get(..).unwrap_or(0)` at every consumer — which is exactly the "empty is
/// not absent" defect (global principle 5) in one line.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TokenKinds(std::collections::BTreeMap<String, u64>);

impl TokenKinds {
    /// The count reported for `id`, or `None` — **never zero for absent**.
    ///
    /// Rust-side this is read by the tests that pin the absence rule; the
    /// production consumer of a reading is the frontend, which receives the map
    /// as JSON. Declared and kept under test anyway, so the next Rust consumer
    /// reaches for an accessor that cannot spell `unwrap_or(0)` by accident.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, id: &str) -> Option<u64> {
        self.0.get(id).copied()
    }

    /// Record a reported count. Callers only call it for a value they actually
    /// received; there is deliberately no "insert a default".
    pub fn set(&mut self, id: &str, tokens: u64) {
        self.0.insert(id.to_string(), tokens);
    }

    /// Whether nothing at all was reported.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The category ids present, in id order. Same posture as [`Self::get`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

/// One lane a turn's usage can be attributed to.
///
/// The `session` / `agent` split is Claude Code's *sidechain* model (a turn on
/// the main transcript vs. one in `<sid>/subagents/*.jsonl`, or an inline
/// `isSidechain:true` line). It is stored in `usage_stat.origin` and rendered
/// as the Usage donut's lanes; declaring it makes both facts a harness's
/// statement rather than core's assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnOrigin {
    /// The stored wire string (`"session"` / `"agent"`). Persisted in the
    /// `usage_stat.origin` column, so a declared id is frozen once written.
    pub id: &'static str,
    /// The lane label a UI renders.
    pub label: &'static str,
    /// True for the lane whose spend is a sub-agent's, rolled up to the
    /// session that spawned it. A harness with no fan-out declares one lane
    /// with `subagent: false`.
    ///
    /// Declared rather than inferred from the id, because the *only* other way
    /// to know which of two lanes is the fan-out one is to recognise the word
    /// `"agent"` — which is exactly the vendor literal locked decision 19
    /// exists to delete. The ingress that rolls a child session's spend up to
    /// its parent (`offload/loopback.rs`) reads this flag and nothing else.
    pub subagent: bool,
}

/// **The shape of a RECORDED turn**: which token categories a harness reports
/// per turn, and which lanes it attributes them to.
///
/// Separate from [`UsageSource`] on purpose, and that separation is the
/// modelling fix (locked decision 19's remainder). Pre-V40-G the categories and
/// the lanes hung off the usage *source* — so OpenCode, which answers
/// `usage_source() == None` because nothing reports its quota or context
/// window, was declared to record no turns either. It does record them: its
/// plugin POSTs per-turn token totals to `/memory/event` and rolls a child
/// session's spend up to `parent_session_id`. Quota is one question ("can this
/// harness tell me how much of my plan is left?") and turn accounting is
/// another ("when cImp writes a `usage_stat` row for this harness, what shape
/// is it?"); a harness may answer either without the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnUsageShape {
    /// The billing categories this harness reports a turn's tokens under. A
    /// category it does not declare is **absent** from a stored turn's
    /// [`TokenKinds`], never present as zero.
    pub token_kinds: &'static [TokenKindSpec],
    /// The lanes a turn's usage can be attributed to, in display order. The
    /// ids are persisted verbatim in `usage_stat.origin`.
    pub origins: &'static [TurnOrigin],
}

impl TurnUsageShape {
    /// The id of the lane a sub-agent's spend rolls into, or `None` when this
    /// harness declares no fan-out lane. First match wins.
    pub fn subagent_origin(&self) -> Option<&'static str> {
        self.origins.iter().find(|o| o.subagent).map(|o| o.id)
    }

    /// The id of the lane a first-party turn belongs to, or `None` when this
    /// harness declares no such lane (which would make it undeclarable —
    /// see the shape test). First match wins.
    pub fn main_origin(&self) -> Option<&'static str> {
        self.origins.iter().find(|o| !o.subagent).map(|o| o.id)
    }

    /// Whether this harness declares `id` as a token category. The read
    /// boundary in `graph/index.rs` asks this per query to decide which stored
    /// columns become entries in a [`TokenKinds`] — a category the harness
    /// never declared must not appear as a structural zero.
    pub fn declares_kind(&self, id: &str) -> bool {
        self.token_kinds.iter().any(|k| k.id == id)
    }
}

/// One quota window's current reading. `used` is 0–100; `resets_at` is an
/// ISO-8601 timestamp (with timezone), or `None` for a window that reports
/// none (a window at 0% often does).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    /// The window's duration as the harness names it (`"(5h)"`).
    pub short: String,
    /// One sentence for the row's tooltip.
    pub description: String,
    /// Percentage of the window consumed, 0–100.
    pub used: f64,
    pub resets_at: Option<String>,
}

/// A live context-window reading.
///
/// Every field is independently absent: a reading is assembled leniently from
/// whatever the harness reported, and **absent must render as unknown, never
/// as zero**. `tokens` carries only the categories that were actually
/// reported (see [`TokenKindSpec`]).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ContextReading {
    /// Percentage of the context window in use (0–100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percentage: Option<f64>,
    /// Percentage still free, as reported rather than derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percentage: Option<f64>,
    /// Tokens currently occupying the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    /// The window's size in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    /// The latest turn's tokens, by declared category id — see [`TokenKinds`].
    #[serde(default, skip_serializing_if = "TokenKinds::is_empty")]
    pub tokens: TokenKinds,
    /// Session metadata the harness reported beside the numbers (session name,
    /// agent/persona, effort, thinking, fast mode). Opaque to core, keyed by
    /// the harness's own names; empty when none was reported.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meta: std::collections::BTreeMap<String, String>,
}

impl ContextReading {
    /// True when at least one *number* is present. Metadata alone (a session
    /// name, an effort string) is not something a context bar can draw, so it
    /// does not make a reading worth showing — "empty is not absent" (global
    /// principle 5).
    pub fn is_substantive(&self) -> bool {
        self.used_percentage.is_some()
            || self.remaining_percentage.is_some()
            || self.total_input_tokens.is_some()
            || self.context_window_size.is_some()
            || !self.tokens.is_empty()
    }
}

/// What one harness's usage source currently reports.
///
/// `windows` empty **and** `context` `None` is a source that exists but has
/// nothing to say yet (no tab of that harness has reported). That is a
/// different answer from [`HarnessPlugin::usage_source`] returning `None`,
/// which is "this harness has no usage source at all" — and the difference is
/// exactly what stops a harness that cannot report quota from rendering as a
/// harness sitting at 0%.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct UsageReading {
    /// The quota windows that have a current reading, in declared order.
    pub windows: Vec<QuotaWindow>,
    /// The live context reading, when one was reported.
    pub context: Option<ContextReading>,
    /// True when everything on the reading is aging — nothing is fresh.
    pub stale: bool,
    /// True when the quota half is present and aging. False when there is no
    /// quota data at all (nothing to dim).
    pub quota_stale: bool,
    /// True when the context half is present and aging.
    pub context_stale: bool,
}

/// Where a harness's usage / quota / context readings come from.
///
/// A trait object rather than a data table because a reading is *read* —
/// Claude's comes off a push file its own status-line child writes, and a
/// future harness's could come off an API or a CHP event. What core needs is
/// the declaration (which windows) plus one call that produces the current
/// reading.
///
/// It does **not** declare token categories or turn lanes any more: those
/// describe a RECORDED turn, not a quota reading, and live on
/// [`TurnUsageShape`] behind [`HarnessPlugin::turn_usage_shape`]. Keeping them
/// here made "records turns" and "reports quota" one answer, which is why a
/// harness that does the first without the second could not say so.
pub trait UsageSource: Sync + Send {
    /// The quota windows this source can report, in display order.
    fn windows(&self) -> &'static [QuotaWindowSpec];
    /// The current reading, or `None` when this source has produced nothing
    /// still worth showing. Must be cheap and must never block for long — a
    /// widget polls it.
    fn read(&self) -> Option<UsageReading>;
    /// Model ids this harness *fabricates* — a pseudo-model stamped on a
    /// locally generated message (an error, an interrupt) that nobody billed
    /// for and that must be excluded from "which model ran this session".
    fn model_sentinels(&self) -> &'static [&'static str] {
        &[]
    }
}

// ── session identity (locked decision 20) ───────────────────────────────────

/// **Which key space a harness's live-session identity lives in.**
///
/// Locked decision 20. The live-session registry used to be one map holding
/// two key spaces at once — a Claude entry keyed by the stable TAB id its
/// transcript tap runs in, an OpenCode entry keyed by the SESSION id its
/// plugin POSTs — which is why a `/memory/event` naming a configured tab id
/// could repoint that tab's session (C-2). Declaring the space makes them two
/// maps instead of one map plus a collision check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionKey {
    /// Identity is owned by the tab: cImp's own reader binds the session and
    /// keys the entry by the tab id it runs in. Such a harness's live session
    /// is never keyed by a value that arrived over the wire.
    Tab,
    /// Identity is the session id the harness reports. Entries are keyed by
    /// that id, which lives in its own space and therefore can never name a
    /// cImp tab.
    Session,
}


/// One CLI subcommand a plugin claims — the flag as it appears in `argv`, and
/// the handler core dispatches to before anything else starts.
///
/// A plain `fn` pointer so the table can be a `const`: the alternative is a
/// `match` on flag strings in `main.rs`, which is the thing locked decision 26
/// deletes.
pub struct Subcommand {
    /// The exact `argv[1]` this claims, e.g. `"--statusline"`.
    pub flag: &'static str,
    /// What runs. Returns when the subcommand is done; core then exits.
    pub run: fn(),
}

/// A harness that is pointed at a local model through a **config file or block
/// cImp writes**, rather than through env or flags.
///
/// A separate trait object rather than more [`HarnessPlugin`] methods because it
/// is a whole sub-surface with one implementor and one caller: keeping it behind
/// [`HarnessPlugin::config_writer`] makes "does this harness have one at all" a
/// question with an answer, instead of a method every plugin has to explain
/// returning `None`.
pub trait ConfigWriter: Sync + Send {
    /// Derive this harness's local-provider block from an offload Local
    /// backend's `server_command`.
    ///
    /// `Err` is a self-contained message naming exactly what the command is
    /// missing, so the Settings button can surface it verbatim.
    fn derive_local_provider(
        &self,
        server_command: &str,
    ) -> crate::error::AppResult<crate::settings::LocalProviderBlock>;
}

/// One env var a harness synthesizes when a tab is pointed at a local provider.
///
/// Declared as `(name, ext_key)` pairs rather than as a rendered sentence so the
/// Settings preview shows the user's OWN values: the frontend reads `ext_key`
/// out of `Settings::harness[<id>].ext` and prints `NAME=<value>`. A `None`
/// key is a credential — rendered `NAME=…`, never with the value in it.
pub struct LocalProviderVar {
    /// The environment variable's name, verbatim.
    pub name: &'static str,
    /// The `ext` key whose value fills it, or `None` for a secret.
    pub ext_key: Option<&'static str>,
    /// Render the row only when `ext_key`'s value is non-empty. The optional
    /// model-alias row is the case: an empty alias means the var is not set at
    /// all, and a preview that showed `NAME=` would be describing a spawn that
    /// does not happen.
    pub only_when_set: bool,
}

/// **The user-facing strings and per-harness UI facts the frontend used to
/// hard-code** (locked decision 27).
///
/// Every field here answers a question some Svelte file used to answer with an
/// `if (tabId === 'claude')` or a sentence naming a product. The rule for what
/// belongs is decision 0's, applied to copy: *would this sentence still be true
/// if both shipped harnesses were deleted?* If not, the harness says it and the
/// window renders it — verbatim, so moving a string here is not a rewrite.
///
/// It is deliberately **prose-free on the core side**: core neither composes nor
/// edits these, it interpolates the two named slots (`{label}`, `{tab}` in
/// [`Self::attribution_template`]; `{path}` in [`Self::attachment_format`]) and
/// prints the rest.
pub struct HarnessAffordances {
    /// The in-session command that starts a fresh conversation (`"/clear"`).
    /// Rendered by the taint/timeline copy that tells a user how to clear a
    /// flagged session. `None` = this harness has no such command and the copy
    /// says "restart the tab" alone.
    pub new_session_command: Option<&'static str>,
    /// When a live session picks up a changed MCP tool list — one clause,
    /// rendered after the harness's label ("refreshes its tool list in the same
    /// session"). `None` = unknown, and the copy omits this harness.
    pub tool_list_refresh: Option<&'static str>,
    /// This harness's OWN web tools, spelled as it spells them (Claude's
    /// `WebFetch`/`WebSearch` are capitalised; OpenCode's are not). The
    /// native-web-visibility copy lists them per harness rather than picking
    /// one spelling and being wrong for the other.
    pub web_tools: &'static [&'static str],
    /// Where this harness keeps its own state, as the user would name it —
    /// what the sandbox copy enumerates when it says which directories a
    /// confined tab still reaches.
    pub state_dirs: &'static [&'static str],
    /// What to tell a user whose tab failed to launch because the binary was
    /// not found. `None` = no hint (the raw error stands alone).
    pub install_hint: Option<&'static str>,
    /// Where that hint points. Rendered as a link after it.
    pub docs_url: Option<&'static str>,
    /// How a pasted image path is written into the prompt. `{path}` is the only
    /// slot. The *instruction* that follows it is model-visible text and comes
    /// from `harness_instructions` (locked decision 24) — this is the line
    /// shape, which the user sees in their own compose box.
    pub attachment_format: &'static str,
    /// The env vars a tab of this harness synthesizes when pointed at a local
    /// provider, in render order. `None` = this harness has no local-provider
    /// control at all, and the Settings form hides the checkbox and shows
    /// [`Self::local_provider_note`] instead.
    pub local_provider: Option<&'static [LocalProviderVar]>,
    /// Why this harness has no local-provider control, for the form to print
    /// where the control would have been.
    pub local_provider_note: Option<&'static str>,
    /// What the Offload card's *register this local backend with the harness*
    /// button does, for the paragraph under it. Only read for a harness that
    /// declares `LocalProviderConfig`.
    pub local_provider_config_note: Option<&'static str>,
    /// How many stacked rows this harness's status-line widget needs in the
    /// bottom strip. Locked decision 19: the 44 px `.status-bar` height was two
    /// rows of Claude Code's 5h + 7d pair, asserted in a stylesheet.
    pub statusline_rows: u8,
    /// V39's delegation attribution line, with `{label}` and `{tab}` slots
    /// (*(0-d)*). One source for the banner, the local echo and the glyph
    /// title. The default is the neutral rendering; a harness overrides it only
    /// if its users expect different wording.
    pub attribution_template: &'static str,
    /// How cImp gets text in front of this harness's model at prompt time
    /// (Claude: a `UserPromptSubmit` hook; OpenCode: a generated plugin). One
    /// clause, rendered after the label.
    pub inject_mechanism: Option<&'static str>,
    /// The command a fresh tab of this harness is seeded with — what the
    /// Settings *Command* field says it defaults to.
    pub default_command: &'static str,
    /// An absolute-path example for that field's hint, for a binary PATH does
    /// not reach.
    pub command_example: Option<&'static str>,
    /// The CSS colour this harness's rows and glyphs are accented with, as a
    /// value the frontend can put straight in a `color:` (a `var(--token,
    /// #fallback)` is expected). Empty = no accent, and the row renders in the
    /// default colour — which is what an unknown harness gets.
    pub accent: &'static str,
    /// Where this harness's model runs, as the graph pulse buckets it:
    /// `"cloud"` for an interactive agent session, anything else for a harness
    /// whose calls should not share that bucket.
    pub tier: &'static str,
}

impl Default for HarnessAffordances {
    /// The neutral answer to every question. A harness that declares nothing
    /// still renders — with no accent, no hints and cImp's own attribution
    /// wording — rather than borrowing another harness's copy.
    fn default() -> Self {
        HarnessAffordances {
            new_session_command: None,
            tool_list_refresh: None,
            web_tools: &[],
            state_dirs: &[],
            install_hint: None,
            docs_url: None,
            attachment_format: "[image] {path}",
            local_provider: None,
            local_provider_note: None,
            local_provider_config_note: None,
            statusline_rows: 0,
            attribution_template: "[delegated by {label} · tab \"{tab}\" · via cImp]",
            inject_mechanism: None,
            default_command: "",
            command_example: None,
            accent: "",
            tier: "cloud",
        }
    }
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

    /// This harness's own name for a [`ToolRole`], or `None` when it serves no
    /// such tool.
    ///
    /// Locked decision 24. The name must be one of this harness's
    /// [`Self::native_tools`] rows — asserted by
    /// `registry::tests::every_declared_tool_role_names_a_native_tool`, so a
    /// renamed tool cannot leave the prompt pointing at a name the harness
    /// stopped serving. `None` renders as a *description* rather than as
    /// another product's id ([`crate::harness::instructions`]), which is the
    /// same fail-closed direction [`Self::native_tools`] takes.
    fn tool_for_role(&self, _role: ToolRole) -> Option<&'static str> {
        None
    }

    /// **Every string cImp puts in front of this harness's model**, rendered in
    /// its vocabulary (locked decision 24).
    ///
    /// The prose is cImp's — it describes cImp's graph tools, cImp's channel,
    /// cImp's delegation contract — so it lives in
    /// [`crate::harness::instructions`] and every shipped plugin returns
    /// `instructions::render_for(<its id>)` out of a `OnceLock`. What the plugin
    /// contributes is the vocabulary: its [`Self::tool_for_role`] names and its
    /// descriptor label.
    ///
    /// **The default is empty, and it is not a silent default**:
    /// `instructions::all_for` falls back to the neutral rendering, and
    /// `instructions::tests::every_harness_declares_every_slot` refuses to let a
    /// registered harness ship with a partial inventory. Overriding this is how
    /// a harness would say something *else* — not how it opts out.
    fn instructions(&self) -> &[crate::harness::instructions::Instruction] {
        &[]
    }

    /// The argument keys this harness's tool payloads carry a [`MemArg`] under,
    /// **in precedence order**.
    ///
    /// Locked decision 16, closing the ledger's `loopback.rs:9124` row: core
    /// used to try `file_path` → `filePath` → `notebook_path` → `path` for
    /// every caller — Claude's snake_case and OpenCode's camelCase merged in one
    /// `match`, so a third harness's payload was mined with two other products'
    /// spellings and a collision between them would have been invisible. Each
    /// harness names its own keys; an empty list records nothing, which is the
    /// same fail-closed direction [`HarnessPlugin::native_tools`] takes.
    ///
    /// **This IS locked decision 24's tool-arg aliasing**, which the milestone
    /// doc spells `native_tools().arg_names()`. Phase C had already landed the
    /// same narrowing keyed by [`MemArg`] rather than by tool name, and Phase E
    /// kept it: the consumer asks "which key carries the target of THIS event",
    /// the event's kind is what decides that, and a second per-tool spelling of
    /// the same answer would be a third vocabulary for one question — the exact
    /// shape both this method and [`crate::harness::native`] exist to remove.
    fn memory_arg_keys(&self, _arg: MemArg) -> &'static [&'static str] {
        &[]
    }

    // ── MCP client specifics (locked decision 25) ───────────────────────────

    /// Add this harness's own capability declarations to the per-session MCP
    /// child's `initialize` result.
    ///
    /// Locked decision 25. `capabilities.experimental["claude/channel"]` is a
    /// key in ONE vendor's namespace, and core wrote it unconditionally for
    /// whichever consumer had session push armed — so a second harness with an
    /// inbound MCP path would have been handed Claude's key. The neutral half
    /// (`protocolVersion`, `tools.listChanged`, `serverInfo`, and the
    /// `instructions` block from [`Self::instructions`]) stays core's; this adds
    /// only what is the harness's own.
    ///
    /// Called only when the child is actually declaring the channel — i.e. when
    /// [`Self::supports_session_push`] is true and the spawn baked the flag in.
    fn decorate_initialize(&self, _result: &mut serde_json::Value) {}

    /// The JSON-RPC notification method a session push is delivered as, or
    /// `None` for a harness with no inbound path.
    ///
    /// `notifications/claude/channel` is Claude Code's spelling, and it is the
    /// twin of [`Self::decorate_initialize`]'s capability key: a client that
    /// declared one and was sent the other would drop every push silently.
    fn push_notification_method(&self) -> Option<&'static str> {
        None
    }

    /// The MCP protocol version the per-session child must answer this
    /// harness's `initialize` with, or `None` to speak cImp's own.
    ///
    /// A PIN, not a preference (milestone invariant 1): Claude Code honours
    /// channel notifications only in the `2025-06-18` era, so the child must
    /// report that version to it whatever cImp itself would otherwise
    /// negotiate. Declared per harness because "which era does the client
    /// honour" is a fact about the client.
    fn mcp_protocol_version(&self) -> Option<&'static str> {
        None
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

    // ── hook ingress (locked decisions 15 and 22) ───────────────────────────

    /// The loopback routes this harness owns.
    ///
    /// Core's router appends these after every CHP-neutral arm, so the harness's
    /// path literals, its payload shapes and its reply envelope all live in
    /// `harness/<id>/`. Empty is the ordinary answer for a harness that only
    /// speaks CHP.
    fn routes(&self) -> &'static [Route] {
        &[]
    }

    /// The identity this harness puts **outside** a request body, if it puts one
    /// there at all.
    ///
    /// Answers `None` for a route this harness does not own, and for one whose
    /// identity rides the CHP envelope — in which case core reads the envelope,
    /// as it does for every other caller.
    fn identity_of_request(
        &self,
        _route: &str,
        _req: &crate::offload::loopback::Request,
    ) -> Option<RequestIdentity> {
        None
    }

    /// The CHP event one of **this harness's own** ingress routes feeds.
    ///
    /// The join the quiet detector needs in order to speak about capabilities
    /// rather than about transports: a harness whose hook body cannot carry a
    /// CHP envelope still reaches the same capability cores, and this is what
    /// says which. `None` for a route this harness does not own, and for one
    /// whose event is not one arbitration can turn off.
    fn chp_event_for_route(&self, _route: &str) -> Option<&'static str> {
        None
    }

    /// The drift-ledger token this harness reports a **quiet** capability
    /// under.
    ///
    /// One bucket per capability, shared with that capability's payload-drift
    /// reports on purpose: a capability that broke either way — a malformed
    /// payload, or a served push that stopped arriving — lands in one place.
    /// Must answer with a token this harness also declares in
    /// [`Self::drift_vocabulary`], which
    /// `ingress::tests::a_quiet_token_is_a_declared_token` checks.
    fn drift_token_for_capability(&self, _capability: &str) -> Option<&'static str> {
        None
    }

    /// Routes whose **identity-less** bodies came from an older artifact of
    /// THIS harness.
    ///
    /// Locked decision 22's "explicit, commented policy line", declared rather
    /// than written into core. Core's blanket answer for a body with no
    /// `agent`/`consumer` is [`crate::harness::DEFAULT_HARNESS`] — a
    /// compatibility statement about the era before those fields existed, when
    /// Claude's shims were the only thing that could have posted. Two routes
    /// invert it because only one harness has ever posted to them at all, and
    /// that asymmetry is load-bearing: reading them as the default harness
    /// would attribute an OpenCode tool gate to a Claude tab.
    ///
    /// Same expiry as `DEFAULT_HARNESS`: new code requires an identity.
    fn legacy_wire_default_routes(&self) -> &'static [&'static str] {
        &[]
    }

    /// One sentence naming **this harness's** mechanism behind `capability`,
    /// for the advisor's fix pointer (locked decision 23).
    ///
    /// Rendered verbatim after a rule's neutral rationale. `None` is an ordinary
    /// answer — a rule with no hint says what it measured and stops, which is
    /// better than a pointer at a mechanism this harness does not have.
    fn drift_hint(&self, _capability: &str) -> Option<&'static str> {
        None
    }

    /// The payload-drift ledger tokens this harness's ingress reports under.
    ///
    /// One `&'static str` bucket per capability, so a caller-supplied string can
    /// never become a ledger key. Core owns the ledger and the bound; the
    /// vocabulary is the harness's, because the names are its hooks'.
    fn drift_vocabulary(&self) -> &'static [&'static str] {
        &[]
    }

    /// How long this harness's out-of-process caller waits for cImp's reply
    /// before abandoning it and starting the tool anyway.
    ///
    /// Core takes `min(all declared) − `[`HOOK_REPLY_MARGIN`] as its own
    /// pre-tool budget ([`hook_reply_budget`]), which is the ordering that makes
    /// cImp's answer the one that decides. `None` means "this harness never
    /// waits on a reply" and does not participate in the minimum.
    fn hook_reply_timeout(&self) -> Option<std::time::Duration> {
        None
    }

    // ── permission-prompt grammar (locked decision 21) ──────────────────────

    /// This harness's TUI permission/question/working prompt rows.
    ///
    /// Data, not code: the detector engine
    /// ([`crate::processing::permission::PermissionDetector`]) is core and
    /// harness-neutral; what it matches on is a *transcription of somebody
    /// else's terminal chrome* and belongs with the harness it was transcribed
    /// from.
    fn permission_patterns(&self) -> &'static [crate::processing::permission::PatternSpec] {
        &[]
    }

    /// One clause for the `_doc` header of the shipped `patterns.json`,
    /// illustrating what this harness's rows use `none_of` for.
    ///
    /// Locked decision 21's last sentence. The header used to read "…that is how
    /// the permission patterns stay off Claude's select menus" from inside
    /// `processing/patterns_file.rs` — a user-facing string in core, naming one
    /// product's TUI. The sentence is worth keeping (it is the only place a
    /// hand-editing user learns what the veto list is FOR), so the harness whose
    /// menus it describes supplies it.
    ///
    /// Composed into the header verbatim and in registry order; a harness that
    /// declares none contributes nothing.
    fn patterns_doc_note(&self) -> Option<&'static str> {
        None
    }

    /// The rows this harness shipped in a **named earlier era** of the on-disk
    /// `patterns.json`, for pristine-file reconciliation.
    ///
    /// A file written by an older cImp is "pristine" when it still equals what
    /// that version wrote; the reconciler therefore needs every historical row
    /// set, not just today's. Keyed by the era tag the writer stamped.
    fn legacy_permission_patterns(
        &self,
        _era: &str,
    ) -> &'static [crate::processing::permission::PatternSpec] {
        &[]
    }


    // ── activity, usage, session identity (locked decisions 18, 19, 20) ─────

    /// How core learns this harness is busy — see [`ActivitySource`].
    ///
    /// The default is [`ActivitySource::OutOfBand`], and that is the
    /// fail-closed direction: a harness that has not declared TUI timings does
    /// not get another harness's spinner model applied to its terminal. The
    /// cost of the default is an avatar that never leaves Idle, which is a
    /// visible absence; the cost of the other default would be an avatar
    /// cycling on somebody else's repaint rate, which reads as a bug in cImp.
    fn activity_source(&self) -> ActivitySource {
        ActivitySource::OutOfBand
    }

    /// This harness's usage / quota / context source, or `None` when it has
    /// none.
    ///
    /// `None` is a first-class answer that a UI must render as *no usage
    /// source*, never as a harness sitting at 0% — locked decision 19. The
    /// default is `None` for the same reason every other default here is the
    /// neutral one: a harness gets a quota widget by declaring a source, not
    /// by existing.
    fn usage_source(&self) -> Option<&'static dyn UsageSource> {
        None
    }

    /// The shape of a turn cImp RECORDS for this harness — see
    /// [`TurnUsageShape`].
    ///
    /// `None` means this harness produces no per-turn usage rows at all, and it
    /// is the neutral default for the same reason [`Self::usage_source`]'s is:
    /// a harness gets a token breakdown by declaring one. Independent of
    /// `usage_source`: OpenCode declares a shape and no source (it records
    /// turns, it reports no quota), and the reverse would be equally legal.
    ///
    /// The read boundary (`graph/index.rs`) resolves a stored session's harness
    /// and asks this which categories to emit. A session whose harness declares
    /// no shape falls back to emitting only the columns that are non-zero — an
    /// undeclared category is never invented.
    fn turn_usage_shape(&self) -> Option<&'static TurnUsageShape> {
        None
    }

    /// Whether this harness's MCP client can be PUSHED to — an inbound path
    /// from the server to the model between turns.
    ///
    /// Locked decision 25. The default is `false`, and it is the fail-closed
    /// one: a harness that cannot receive a push must not have the server
    /// declare a channel capability it will then drop on the floor. cImp's
    /// child gates its `initialize` declaration AND its subscription on this,
    /// so both halves of the registration move together.
    fn supports_session_push(&self) -> bool {
        false
    }

    /// Which key space this harness's live-session identity lives in — see
    /// [`SessionKey`].
    ///
    /// The default is [`SessionKey::Session`]: an id an undeclared harness
    /// hands cImp is its own session id and lands in the session space, where
    /// it cannot name a cImp tab. Defaulting to [`SessionKey::Tab`] would let
    /// a wire value key the tab space, which is the C-2 hazard the declaration
    /// exists to remove.
    fn session_key_space(&self) -> SessionKey {
        SessionKey::Session
    }

    /// Activity-store `tool` names this harness's own readers file drift
    /// reports under (`subagent_drift` is Claude's).
    ///
    /// Core owns the ring and the advisor rule; which rows are *this harness's*
    /// drift is the harness's statement. Without it the rows are attributable
    /// only to `source: "harness"` and every per-harness signal has to guess —
    /// which is what the pre-V40 code did by handing them all to the default
    /// harness.
    fn drift_report_tools(&self) -> &'static [&'static str] {
        &[]
    }

    /// Extra CLI subcommands this harness needs cImp itself to answer.
    ///
    /// Claude Code runs `cimp --statusline` and pipes its status-line JSON to
    /// stdin; the flag, the stdin/stdout contract and the shell quoting around
    /// it are all Claude's, so `main.rs` no longer knows the flag exists — it
    /// asks every registered plugin whether it claims `argv[1]`.
    ///
    /// Handled **before** any Tauri/audio/settings init, so a handler must be
    /// instant, must never spin up the GUI, and must terminate the process
    /// itself or return.
    fn subcommands(&self) -> &'static [Subcommand] {
        &[]
    }

    // ── CLI vocabulary and config writers (locked decision 26) ──────────────

    /// The flags with which this harness's CLI selects a session, so cImp can
    /// tell "the user pinned a session" from "cImp may pin one".
    ///
    /// Locked decision 26. `["--session-id", "--resume", "-r", "--continue",
    /// "-c", "--fork-session", "--from-pr"]` is Claude Code's vocabulary and
    /// nobody else's; the `--flag=value` form is matched as well as the
    /// two-token one by the plugin that declares it. Empty means "this harness
    /// selects its session some other way", which is OpenCode's answer.
    fn session_selector_flags(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether a tab of this harness may be enabled right now — `Err` carries
    /// the **install hint** the UI appends to its refusal.
    ///
    /// Locked decision 26. This was an `if enabling_opencode { resolve_command
    /// ("opencode") }` in the tab-lifecycle command, with Claude's exemption
    /// stated as a comment ("intentionally not gated — it's the app's own front
    /// end"). An exemption in a comment is not one a third harness inherits
    /// correctly: it would have been ungated by accident. Claude declares its
    /// `Ok` here instead, so *not gated* is a decision on the record.
    ///
    /// **Blocking** — an implementation resolves a binary. Callers run it off
    /// the async runtime.
    fn preflight(&self) -> Result<(), &'static str> {
        Ok(())
    }

    /// Whether killing this harness's child must reap the whole process TREE.
    ///
    /// `opencode serve` is a Bun binary that forks children (observed: two
    /// grandchildren per server), so a bare kill leaves a live HTTP server bound
    /// to the probe's loopback port. The primitive stays in `procutil`
    /// (`reap_probe_child`); what is declared here is the REQUIREMENT, so a
    /// harness that needs it says so rather than the reaping code naming a
    /// product.
    ///
    /// **Defaults to `true`** — the direction where being wrong is cheap. A
    /// leaked child holding a port outlives the run that made it and is
    /// invisible until the next bind fails; a redundant tree kill costs one
    /// process spawn on a path that is already tearing a child down. A harness
    /// whose child provably has no descendants may declare `false`.
    fn needs_tree_reap(&self) -> bool {
        true
    }

    /// Whether a freshly spawned tab of this harness paints startup chrome that
    /// looks like a completed turn.
    ///
    /// Claude Code's welcome banner cycles a new tab `Idle → Thinking → Idle` as
    /// it prints, which the notification manager would otherwise announce as
    /// "your task finished" before the user has typed anything. The guard (a tab
    /// stays silent until it has been driven to `Listening` once) is core's; the
    /// FACT that a harness needs it is the harness's.
    ///
    /// Defaults to `true` — the fail-safe direction: the cost of the guard is a
    /// suppressed announcement nobody asked for, and the cost of missing it is a
    /// spurious one on every tab spawn.
    fn emits_startup_chrome(&self) -> bool {
        true
    }

    /// This harness's rows in the external-process spawn ledger
    /// ([`crate::spawn_ledger`]), or none.
    ///
    /// The ledger is a reviewed table whose tripwire scans the whole tree, so
    /// the rows must exist — but a row for `opencode serve --port <free>
    /// --hostname 127.0.0.1` is a sentence about one product's CLI, and it
    /// belongs beside that product's probe. The tripwire consumes core's rows
    /// and every plugin's together.
    fn spawn_sites(&self) -> &'static [crate::spawn_ledger::SpawnSite] {
        &[]
    }

    /// How this harness is pointed at a local provider, or `None` when it has no
    /// such config to write.
    ///
    /// Locked decision 26. Claude's half is env synthesis and already rides
    /// [`Self::compose_env`]; OpenCode's is a `local-llama` provider block
    /// derived from the offload server's command line, which core used to own in
    /// `offload/server.rs` and which core still needs to CALL (the Settings
    /// "Add to OpenCode" button). It asks here instead of naming the harness.
    fn config_writer(&self) -> Option<&'static dyn ConfigWriter> {
        None
    }

    // ── the frontend's affordances (locked decision 27) ────────────────────

    /// The user-facing strings and per-harness UI facts this harness supplies
    /// to the window — see [`HarnessAffordances`].
    ///
    /// Reaches the frontend inside `harness_list`, once at startup. The default
    /// is the neutral one: a harness that declares nothing renders with cImp's
    /// own wording and no accent, which is a visible absence rather than
    /// another product's copy under this one's name.
    fn affordances(&self) -> HarnessAffordances {
        HarnessAffordances::default()
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
    /// **Every declared TUI tuning is internally ordered.**
    ///
    /// The stall backstop has to outlast the marker path or the avatar is
    /// released while the busy footer is still on screen; the marker grace has
    /// to outlast the quiet window or a footer that blinks between sub-agent
    /// batches settles the avatar to Idle. Before V40 those two orderings were
    /// stated in prose beside constants in two different files, so nothing
    /// checked them — and a new harness declaring its own tuning has no prose
    /// to read at all.
    #[test]
    fn every_stall_backstop_outlasts_its_marker_path() {
        for id in crate::harness::registry::all() {
            let Some(plugin) = id.plugin() else { continue };
            let ActivitySource::TuiMarkers(t) = plugin.activity_source() else {
                continue;
            };
            assert!(
                t.subagents_stall > t.working_stale,
                "{id}: the sub-agent stall backstop must conclude AFTER the marker path"
            );
            assert!(
                t.marker_grace > t.quiet,
                "{id}: a marker gap shorter than the grace must not release on quiet alone"
            );
            assert!(
                t.working_stale > t.quiet,
                "{id}: the stale valve must be the last resort, not the first"
            );
            assert!(
                !t.burst_min.is_zero() && !t.quiet.is_zero(),
                "{id}: a zero timer is not a fallback, it is a flicker"
            );
        }
    }

    /// **A harness with no usage source says so, and says it exactly once.**
    ///
    /// Locked decision 19's whole point: "no usage source" and "a source that
    /// has reported nothing" are different answers, and neither of them is a
    /// quota of zero. OpenCode is the standing example — its plugin posts token
    /// totals to `/memory/event`, but nothing reports a subscription quota or a
    /// context window, so its widget must be absent rather than empty.
    #[test]
    fn a_harness_without_a_usage_source_answers_none_not_zeros() {
        let with: Vec<_> = crate::harness::registry::all()
            .filter(|h| h.plugin().and_then(|p| p.usage_source()).is_some())
            .collect();
        let without: Vec<_> = crate::harness::registry::all()
            .filter(|h| h.plugin().and_then(|p| p.usage_source()).is_none())
            .collect();
        assert!(
            !with.is_empty(),
            "no harness declares a usage source; the widget has no data path at all"
        );
        assert!(
            !without.is_empty(),
            "every harness declares a usage source — if that becomes true, the `None` arm is \
             untested and the widget's absence path stops being exercised"
        );
        for id in with {
            let source = id.plugin().unwrap().usage_source().unwrap();
            assert!(
                !source.windows().is_empty(),
                "{id}: a usage source that declares no window can report nothing"
            );
            let mut ids: Vec<&str> = source.windows().iter().map(|w| w.id).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(n, ids.len(), "{id}: two windows share an id");
            for w in source.windows() {
                assert!(
                    !w.label.is_empty() && !w.short.is_empty() && !w.description.is_empty(),
                    "{id}/{}: a window a UI cannot label is a row of numbers with no meaning",
                    w.id
                );
            }
        }
    }

    /// **Recording turns and reporting quota are separate declarations.**
    ///
    /// The V40 Phase G split (locked decision 19's remainder). Before it,
    /// `token_kinds()` / `origins()` hung off [`UsageSource`], so OpenCode —
    /// which posts per-turn tokens to `/memory/event` and rolls a child
    /// session's spend up to its parent, but reports no quota and no context
    /// window — was modelled as recording nothing. This pins both halves:
    /// OpenCode still answers *no usage source* (live-verify 14 reads that
    /// answer, and it must not become a widget at 0%), and it now declares the
    /// turn shape its rows actually have.
    #[test]
    fn a_harness_can_record_turns_without_reporting_quota() {
        let shaped: Vec<_> = crate::harness::registry::all()
            .filter(|h| h.plugin().and_then(|p| p.turn_usage_shape()).is_some())
            .collect();
        assert!(
            !shaped.is_empty(),
            "no harness declares a turn shape; every stored usage row would fall back to non-zero-columns-only and the declaration would have no producer"
        );
        for id in &shaped {
            let shape = id.plugin().unwrap().turn_usage_shape().unwrap();
            assert!(
                !shape.token_kinds.is_empty(),
                "{id}: a turn shape with no token category describes no row"
            );
            assert!(
                !shape.origins.is_empty(),
                "{id}: a turn shape with no lane cannot attribute a stored row"
            );
            let mut kinds: Vec<&str> = shape.token_kinds.iter().map(|k| k.id).collect();
            let n = kinds.len();
            kinds.sort_unstable();
            kinds.dedup();
            assert_eq!(n, kinds.len(), "{id}: two token categories share an id");
            for k in shape.token_kinds {
                assert!(!k.label.is_empty(), "{id}: token category `{}` has no label", k.id);
            }
            let mut lanes: Vec<&str> = shape.origins.iter().map(|o| o.id).collect();
            let n = lanes.len();
            lanes.sort_unstable();
            lanes.dedup();
            assert_eq!(n, lanes.len(), "{id}: two turn origins share an id");
            for o in shape.origins {
                assert!(!o.label.is_empty(), "{id}: turn origin `{}` has no label", o.id);
            }
            // A harness with no fan-out declares ONE lane with `subagent:
            // false`; a harness with fan-out declares that lane plus the
            // roll-up one. Either way the non-fan-out lane must exist — it is
            // what `/memory/event` attributes a parent-less turn to, and a
            // shape without one records nothing rather than guessing.
            assert!(
                shape.main_origin().is_some(),
                "{id}: every declared lane is a sub-agent lane, so a first-party turn has nowhere to go"
            );
        }
        // The standing example, named by id because this assertion IS the
        // separation: OpenCode records turns and reports no quota.
        let oc = crate::harness::HarnessId::from_id("opencode")
            .expect("opencode is a registered harness");
        let p = oc.plugin().expect("opencode has a plugin");
        assert!(
            p.usage_source().is_none(),
            "opencode gained a usage source; live-verify 14 reads `harness_usage(\"opencode\")` answering NO usage source, not a widget at 0%"
        );
        let shape = p
            .turn_usage_shape()
            .expect("opencode records per-turn tokens through /memory/event");
        assert!(
            shape.subagent_origin().is_some(),
            "opencode's plugin rolls a child session's spend up to `parent_session_id`, so it must declare the lane that roll-up lands in"
        );
    }

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

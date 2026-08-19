//! V38 Phase A — the **plugin manifest**: the versioned, on-disk description of
//! a set of tools cImp can run, and the validation that decides whether one is
//! well-formed.
//!
//! A manifest is **attacker-controlled input** the moment plugins are
//! installable (decision 7: a plugin carries no binaries, but it does carry an
//! argv template, an env block, and a request for sandbox grants). So this file
//! is a parse boundary in the strict sense: everything the loader hands on is
//! validated here, post-hoc, against a closed set of enums and tokens — never
//! "documented" and trusted. The three rules that make that hold:
//!
//! * **Closed enums, never free-form.** `runtime` selects from a table cImp
//!   owns (`sandbox::RUNTIME_PROFILES`); the worst a lying manifest achieves is
//!   a grant the user can see named at enable time. A free-form runtime path
//!   would make the manifest a grant-widening primitive.
//! * **Provenance is stamped, never claimed.** `builtin` is not a manifest
//!   field, in any file, embedded or scanned ([`ValidationError::BuiltinField`]).
//!   The loader stamps [`Provenance`] and the security-relevant gates
//!   (`parser`, the reserved name prefix) key off that flag, never off a name
//!   string.
//! * **Unknown fields are errors** (`deny_unknown_fields`). A typo'd field that
//!   silently does nothing is a misconfiguration that surfaces as a *behaviour*
//!   difference weeks later; at a versioned schema boundary the honest answer
//!   is to refuse the file and say which key was not understood.
//!
//! Shape note: the manifest deliberately **rhymes with the two config shapes
//! that already exist** — `audit::adapters::Adapter` (argv template, transport,
//! findings exit codes, applicability gates, forced env) and
//! `checks::CheckDef` (cmd, parser, cwd, report_file, timeout, env). There is no
//! third config vocabulary, and the check-kind path literally reuses
//! [`CheckDef::validate`] rather than re-deriving its confinement rules beside
//! it.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::checks::{CheckDef, ParserKind};

/// The only manifest schema version this build understands.
///
/// Validated, not advisory: a file declaring anything else is a load error
/// rather than a best-effort parse. A newer plugin dropped next to an older
/// cImp must fail loudly and name the version, because the alternative — a
/// partial parse of a schema whose meaning changed — is exactly the
/// silent-misconfiguration failure this boundary exists to prevent.
pub const MANIFEST_VERSION: u32 = 1;

/// The plugin-name prefix reserved for cImp's own shipped plugins.
///
/// A user plugin claiming it is rejected ([`ValidationError::ReservedName`]).
/// This is a **name-space** reservation for readability, NOT the built-in gate:
/// the gate is [`Provenance`], which the loader stamps. A future built-in whose
/// name did not start with `cimp-` would still be built-in, and a user plugin
/// named `cimp-evil` would still be a user plugin — it just never gets to
/// present itself as one of ours in the settings list.
pub const RESERVED_NAME_PREFIX: &str = "cimp-";

/// Largest manifest this build will read, in bytes.
///
/// A manifest is a short declaration — the roomiest plausible one (every built-in
/// adapter re-expressed in one file) is a few tens of kilobytes — so a megabyte
/// is two orders of magnitude of headroom and still bounds what a dropped file
/// can cost. Without a cap, `plugins/` is a directory anything running as the
/// user can write into and the startup scan `read_to_string`s every `.json` in
/// it: one enormous file turns launch into an OOM. Enforced in BOTH places that
/// can see a size — [`crate::plugins::loader`] checks the directory entry's
/// length *before* reading (that is the resource guard), and [`parse`] checks
/// the text it was handed (that is the contract, and it covers the embedded
/// built-in path too).
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Longest identity or display string a manifest may carry, in characters.
///
/// Ids, versions and labels are rendered in a settings list, joined into
/// `name@version/tool-id` keys, and stamped into Events rows; a 50 KB "label"
/// is not a name, it is a payload, and every consumer downstream would have to
/// decide how to truncate it. One hundred characters is far past any honest
/// name and short enough that no renderer has to think about it.
pub const MAX_NAME_CHARS: usize = 100;

/// The widest per-tool timeout a manifest may request, in seconds (24 hours).
///
/// The floor is 1: `timeout_secs: 0` reads as "no timeout" to a human and as
/// "kill it immediately" to a scheduler, so it is refused rather than assigned
/// one of those meanings. The ceiling exists because the value becomes a
/// wall-clock budget a pipeline waits on — a typo'd `86400000` would park a
/// scan for three years, and "the manifest said so" is not a reason to.
pub const MAX_TIMEOUT_SECS: u64 = 86_400;

/// Where a manifest came from — **stamped by the loader**, never read from the
/// file.
///
/// Two rules key off this and nothing else (R3): a user plugin may not claim
/// the [`RESERVED_NAME_PREFIX`], and an audit/security-kind tool in a user
/// plugin must declare the `sarif` parser (decision 3's one parse boundary).
/// The legacy per-tool parsers survive only for the embedded set Phase E
/// introduces, which is the only thing that will ever carry
/// [`Provenance::Builtin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// Scanned from `<exe-dir>/plugins/`. The default posture: least trusted.
    User,
    /// Embedded in the binary (`include_str!`), parsed through this same
    /// validator. Nothing produces this yet — Phase E does.
    Builtin,
}

/// The capability kind of one tool: which pipeline consumes its output, and
/// therefore what cImp guarantees around the spawn (decision 2). Exactly one
/// per tool, and orthogonal to the *category* a plugin files it under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// SARIF findings into the `quality_audit` fan-out.
    Audit,
    /// SARIF findings into the `security_audit` fan-out.
    Security,
    /// Structured diagnostics into `run_check`.
    Check,
    /// Raw output via `run_command`.
    Command,
}

impl ToolKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ToolKind::Audit => "audit",
            ToolKind::Security => "security",
            ToolKind::Check => "check",
            ToolKind::Command => "command",
        }
    }

}

/// Which cImp-owned sandbox runtime profile this tool needs.
///
/// A **request that cImp stamp grants from a table it owns**
/// (`sandbox::RUNTIME_PROFILES`), not a path — see the module docs. `None` is
/// the positive statement "single static binary"; `Auto` keeps V33's inference
/// (which also stays on as a cross-check: a declaration that disagrees with
/// detection is drift worth surfacing, not a tie to break silently — Phase C).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeReq {
    /// A single static binary: its own directory is the whole grant.
    None,
    Python,
    Node,
    Java,
    Dotnet,
    Go,
    Rust,
    /// Infer from the resolved program (V33's `RuntimeProfile` detection).
    #[default]
    Auto,
}

/// The row-and-message spelling of each variant, pinned against the serde wire
/// name by `enum_wire_names_and_as_str_agree`. Its consumers are Phase C's
/// sandbox rows, so it is exercised by that test alone until then — kept here
/// because the vocabulary is what this phase defines.
#[allow(dead_code)]
impl RuntimeReq {
    pub const fn as_str(self) -> &'static str {
        match self {
            RuntimeReq::None => "none",
            RuntimeReq::Python => "python",
            RuntimeReq::Node => "node",
            RuntimeReq::Java => "java",
            RuntimeReq::Dotnet => "dotnet",
            RuntimeReq::Go => "go",
            RuntimeReq::Rust => "rust",
            RuntimeReq::Auto => "auto",
        }
    }
}

/// What cImp does when it **cannot** put this tool inside the OS boundary.
///
/// A different question from [`RuntimeReq`], and deliberately orthogonal: a
/// Python tool may sandbox perfectly, and a static binary may need egress and
/// declare `Unsupported`. The default is the safe one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxReq {
    /// Refuse to run outside the boundary. The default — an author who has not
    /// thought about it gets the safe answer.
    #[default]
    Required,
    /// Run degraded, with a visible `sandbox` Events row saying so.
    Optional,
    /// Runs outside the boundary as an informed user choice, with a visible
    /// row. Exists so that case is a *stated* one rather than the mysterious
    /// failure V33 shipped before those rows existed.
    Unsupported,
}

/// The row-and-message spelling of each variant, pinned against the serde wire
/// name by `enum_wire_names_and_as_str_agree`. Its consumers are Phase C's
/// sandbox rows, so it is exercised by that test alone until then — kept here
/// because the vocabulary is what this phase defines.
#[allow(dead_code)]
impl SandboxReq {
    pub const fn as_str(self) -> &'static str {
        match self {
            SandboxReq::Required => "required",
            SandboxReq::Optional => "optional",
            SandboxReq::Unsupported => "unsupported",
        }
    }
}

/// Where an audit/security-kind tool delivers its SARIF. Mirror of
/// `audit::adapters::Transport` on the wire (`"stdout"` / `"report_file"`);
/// kept as its own type because the manifest is a serialized contract and the
/// adapter table is a compiled one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// SARIF on the child's stdout.
    #[default]
    Stdout,
    /// SARIF written to a report file whose path cImp substitutes for the
    /// `{report}` token.
    ReportFile,
}

/// How much of the ingest contract a tool's output must satisfy — **a built-in
/// declaration, refused in a scanned file**.
///
/// Decision 3 gives every plugin's findings one parse boundary and one
/// substantiveness rule: output that is empty, unparseable, or not a SARIF log
/// is a tool ERROR, never a clean scan. That rule is what stops a blank
/// artifact reading as "no problems found".
///
/// The fourteen tools cImp ships predate it, and their semantics were measured
/// against the real binaries rather than designed: gitleaks writes **no report
/// at all** on a clean run, and cppcheck exits 0 whether or not it found
/// anything. Retro-fitting the strict gate onto them would turn a clean gitleaks
/// scan into a tool failure — a regression dressed as a hardening. So the
/// embedded manifests say so out loud, in one field, once.
///
/// A **user** plugin may never select it ([`ValidationError::BuiltinOnlyField`]),
/// because the strict gate is the only thing standing between a blank artifact
/// and a reassuring zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestReq {
    /// The pre-V38 built-in semantics: whatever the tool wrote is what it
    /// meant, including nothing.
    Grandfathered,
}

/// The wire spelling, pinned against serde's by the drift test that checks
/// docs/TOOL-PLUGINS.md § 2.5 spells the value the way this enum does.
///
/// `allow(dead_code)` because that test is its only caller: unlike
/// [`RuntimeReq`] and [`SandboxReq`], no Events row names an ingest gate — the
/// gate shows up as a tool ERROR with a reason, which is the useful thing to
/// say. Kept because the doc has to spell the value somewhere, and a hand-typed
/// string in a test would be the second spelling this exists to prevent.
#[allow(dead_code)]
impl IngestReq {
    pub const fn as_str(self) -> &'static str {
        match self {
            IngestReq::Grandfathered => "grandfathered",
        }
    }
}

/// The audit-era parsers that are NOT (yet) `checks::ParserKind` variants.
///
/// They exist on the wire only so Phase E's embedded built-in manifests can be
/// expressed before R2 folds these into `ParserKind` — a user plugin can never
/// select one (see [`validate`]). Deliberately a tiny closed list rather than a
/// free-form string: this is the escape hatch, and an escape hatch with no
/// boundary is a hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyAuditParser {
    TyposJsonl,
    KnipJson,
    MacheteText,
}

impl LegacyAuditParser {
    const WIRE: &'static [(&'static str, LegacyAuditParser)] = &[
        ("typos-jsonl", LegacyAuditParser::TyposJsonl),
        ("knip-json", LegacyAuditParser::KnipJson),
        ("machete-text", LegacyAuditParser::MacheteText),
    ];

    fn from_wire(s: &str) -> Option<Self> {
        Self::WIRE.iter().find(|(w, _)| *w == s).map(|(_, k)| *k)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            LegacyAuditParser::TyposJsonl => "typos-jsonl",
            LegacyAuditParser::KnipJson => "knip-json",
            LegacyAuditParser::MacheteText => "machete-text",
        }
    }
}

/// A manifest's `parser` value: any `checks::ParserKind` wire name, or one of
/// the three [`LegacyAuditParser`] names.
///
/// Deserialized through the string rather than as an untagged enum so a typo
/// produces a message naming the field and the offending value instead of
/// serde's "data did not match any variant". `ParserKind`'s own names are never
/// duplicated here — they are resolved through `ParserKind`'s own `Deserialize`
/// — so adding a parser there cannot leave this list stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestParser {
    Kind(ParserKind),
    Legacy(LegacyAuditParser),
}

impl ManifestParser {
    /// The SARIF parser — decision 3's one parse boundary, and the only value a
    /// user plugin's audit/security tool may name.
    pub const SARIF: ManifestParser = ManifestParser::Kind(ParserKind::Sarif);

    fn from_wire(s: &str) -> Option<Self> {
        if let Some(l) = LegacyAuditParser::from_wire(s) {
            return Some(ManifestParser::Legacy(l));
        }
        serde_json::from_value::<ParserKind>(serde_json::Value::String(s.to_string()))
            .ok()
            .map(ManifestParser::Kind)
    }

    pub fn as_wire(self) -> String {
        match self {
            ManifestParser::Kind(k) => serde_json::to_value(k)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "?".to_string()),
            ManifestParser::Legacy(l) => l.as_str().to_string(),
        }
    }
}

impl Serialize for ManifestParser {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for ManifestParser {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ManifestParser::from_wire(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown `parser` value `{s}` (expected a checks parser name such as `sarif`, \
                 `cargo-json`, `tsc`, `generic-gcc`, …)"
            ))
        })
    }
}

/// One declared, user-settable variable. The manifest declares what the
/// settings pane renders — these declarations ARE the rendered fields
/// (decision 10) — and `{var:NAME}` argv tokens may reference nothing else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableDecl {
    /// `[A-Za-z0-9_-]+`, referenced as `{var:NAME}`.
    pub name: String,
    /// The label the settings pane shows.
    pub label: String,
    /// Value used when the user has set none. `None` means "no value until the
    /// user supplies one" — a distinct state from an empty-string default,
    /// which is a value.
    #[serde(default)]
    pub default: Option<String>,
}

/// The project shape a tool applies to — an owned mirror of
/// `audit::adapters::Applicability` (which is `static`). Both lists empty = no
/// gate, exactly as there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Applicability {
    pub extensions: Vec<String>,
    pub markers: Vec<String>,
}

/// One category: the **management** dimension (decision 2). Carries zero
/// contract weight — a tool behaves identically regardless of which category
/// presents it — so nothing outside settings may read this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryDecl {
    pub id: String,
    pub label: String,
    /// Tool ids, all of which must be declared in the same manifest.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// One tool a plugin declares.
///
/// A single flat struct rather than a `kind`-tagged union on purpose: the
/// kind-specific fields are checked against the kind in [`validate`], so a
/// field that belongs to a *different* kind is a loud error ("`cmd` is a
/// check-kind field") rather than a key serde silently drops. A tagged union
/// would make the same mistake deserialize cleanly into a variant missing the
/// field, which is the quieter failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolManifest {
    /// `[A-Za-z0-9_-]+`, unique within the plugin. Namespaced globally as
    /// `name@version/id` — see `loader::LoadedPlugin::tool_key`.
    pub id: String,
    /// What the settings pane calls it.
    pub label: String,
    /// One line saying what the tool is FOR, shown beside the label. Optional,
    /// and worth writing: a settings list of names alone makes the user look
    /// each one up, and the author is the person who knows.
    #[serde(default)]
    pub description: Option<String>,
    pub kind: ToolKind,

    // ── sandbox posture (APPROVED 2026-08-19; every tool kind) ──────────────
    #[serde(default)]
    pub runtime: RuntimeReq,
    #[serde(default)]
    pub sandbox: SandboxReq,
    /// Absolute paths this tool needs granted beyond its runtime profile.
    ///
    /// Validated here only for *shape* (absolute, no `..`). The screening that
    /// matters — V33's `extra_grant_refusal` rules, and showing these to the
    /// user as a permission at enable time — is Phase C, at enable/spawn time,
    /// because that is where the machine's real paths and the user's consent
    /// both exist.
    #[serde(default)]
    pub extra_grants: Vec<String>,

    // ── declared settings surface (every kind) ──────────────────────────────
    #[serde(default)]
    pub variables: Vec<VariableDecl>,
    /// Whether the settings pane offers a free-form "extra CLI parameters"
    /// field for this tool (the `extra_args` successor). Default `false`: a
    /// tool gets an appendable argv only when its author says it tolerates one.
    #[serde(default)]
    pub parameters_allowed: bool,
    /// Whether this tool is ON before the user has ever touched it. Default
    /// `true` — installing a plugin is the consent that matters, and a second
    /// invisible "and now switch each one on" step only teaches people to click
    /// past it.
    ///
    /// `false` is for a tool whose author knows it is expensive or intrusive
    /// enough that nobody should get it by accident: the built-in
    /// `dotnet-analyzers` runs a real build (restores packages, writes `obj/`
    /// and `bin/`) and `semgrep-quality` fetches its ruleset over the network.
    /// Both were default-disabled before V38 and stay so, which is the whole
    /// reason this is a field rather than a constant `true`.
    ///
    /// A DEFAULT, not a lock: the moment the user stores a state for the tool,
    /// that state wins in both directions.
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
    /// Hard timeout. `None` = the consuming pipeline's default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Environment forced onto the child. An ordered list, not a map, so the
    /// settings/manifest diff stays deterministic — `CheckDef::env`'s reason.
    #[serde(default)]
    pub env: Vec<(String, String)>,

    // ── audit / security kinds ──────────────────────────────────────────────
    /// The argv template, tokens `{root}` / `{report}` / `{var:NAME}` (and
    /// `{{` for a literal brace). Program argv only — the executable itself is
    /// never in the manifest (decision 7: the user supplies every path).
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub transport: Option<Transport>,
    /// Non-zero exits that still mean "ran fine, here are findings"
    /// (`Adapter::findings_exit_codes`).
    #[serde(default)]
    pub findings_exit_codes: Vec<i32>,
    #[serde(default)]
    pub applicability: Applicability,

    // ── check kind (mirrors `CheckDef`) ─────────────────────────────────────
    /// The full command line (`CheckDef::cmd`).
    #[serde(default)]
    pub cmd: Option<String>,
    /// Run in this directory instead of the project root; relative, confined.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Parse THIS file after the run instead of stdout; relative, confined.
    #[serde(default)]
    pub report_file: Option<String>,
    /// The `regex-custom` parser's pattern.
    #[serde(default)]
    pub pattern: Option<String>,

    // ── audit/security AND check kinds ──────────────────────────────────────
    /// `None` resolves to the kind's default: `sarif` for audit/security,
    /// `ParserKind`'s own default for a check.
    #[serde(default)]
    pub parser: Option<ManifestParser>,

    // ── built-in only (refused in a scanned file, `builtin`'s mechanism) ─────
    //
    // These four are not a back door. Each states a fact that is true of the
    // fourteen tools cImp ships and of nothing a user can drop in a folder, and
    // each is refused by [`ValidationError::BuiltinOnlyField`] keyed on the
    // loader's [`Provenance`] stamp — never on a name.
    /// Relax the output gate to the pre-V38 built-in semantics. See
    /// [`IngestReq`] for why this exists and why a user plugin may not have it.
    #[serde(default)]
    pub ingest: Option<IngestReq>,
    /// The bare command name a built-in resolves through `ebin` → `PATH` when
    /// the user has configured no explicit path.
    ///
    /// **The one place decision 10's "no automatic PATH resolution" is relaxed,
    /// and only for the built-in tier.** That rule protects the user from cImp
    /// guessing a binary for a definition a stranger wrote; it is not an
    /// argument against cImp resolving `gitleaks` for the gitleaks adapter it
    /// has shipped since V23. Leaving the fourteen inert on upgrade would be a
    /// regression wearing a hardening's clothes. It is a *name*, never a path:
    /// what it selects is still whatever the machine says, exactly as before.
    #[serde(default)]
    pub command: Option<String>,
    /// A `node_modules/.bin` shim name to prefer over a global install (eslint,
    /// knip). Consulted only when the user configured no path.
    #[serde(default)]
    pub project_local_bin: Option<String>,
    /// The argv template used when the scan root is **not** a git repository
    /// (gitleaks' `dir` form against its `git` form). Empty = this tool does not
    /// care, and `argv` is used for both.
    ///
    /// Built-in only because it is a fact about one shipped scanner rather than
    /// a vocabulary plugins need: a user plugin that wants the distinction makes
    /// it inside the wrapper it already points cImp at, and a second template on
    /// every manifest would double the surface every argv rule has to cover.
    #[serde(default)]
    pub dir_argv: Vec<String>,

    /// **Never an author claim.** Declared here only so a file carrying it gets
    /// [`ValidationError::BuiltinField`] instead of serde's generic
    /// "unknown field", which would read as a typo rather than as the
    /// provenance forgery it is.
    ///
    /// Never serialized: a validated manifest always has `None` here, and a
    /// permanently-null field in the snapshot DTO would invite a Phase B reader
    /// to treat it as the provenance flag. The real one is
    /// `LoadedPlugin::provenance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<serde_json::Value>,
}

/// serde's `default` for a `bool` is `false`; a field whose safe answer is
/// `true` needs a function. Named after the settings crate's, for its reason.
fn default_true() -> bool {
    true
}

/// One plugin's manifest file (`<exe-dir>/plugins/<anything>.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Must equal [`MANIFEST_VERSION`].
    pub manifest_version: u32,
    /// Identity, half 1. `[A-Za-z0-9_-]+`.
    pub name: String,
    /// Identity, half 2. Both mandatory (decision 9) — there is no "unversioned
    /// plugin", because the dup rule and the `name@version` namespace both need
    /// a version to exist.
    pub version: String,
    /// Display name; falls back to `name`.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub categories: Vec<CategoryDecl>,
    #[serde(default)]
    pub tools: Vec<ToolManifest>,
    /// See [`ToolManifest::builtin`] — same reason, plugin level, and likewise
    /// never serialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<serde_json::Value>,
}

impl PluginManifest {
    /// `name@version` — the plugin's global identity and the namespace its tool
    /// ids live in.
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

}

/// Everything that can make a manifest invalid, as data rather than as free
/// strings — so the loader, the Events rows and (Phase B) the settings error
/// state all render one vocabulary, and so a test can assert *which* rule
/// fired rather than pattern-matching prose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    /// The file did not parse as JSON, or a field had the wrong type.
    Syntax(String),
    /// The file is larger than [`MAX_MANIFEST_BYTES`]. Named in bytes, because
    /// "too big" without a number leaves the author guessing by how much.
    Size { bytes: u64 },
    /// `manifest_version` is not [`MANIFEST_VERSION`].
    Version { found: u32 },
    /// `timeout_secs` is 0 or above [`MAX_TIMEOUT_SECS`].
    Timeout { tool: String, found: u64 },
    /// `name`/`version`/`id`/`label` missing, empty, too long
    /// ([`MAX_NAME_CHARS`]), duplicated, or outside the id charset.
    Identity(String),
    /// A user plugin claimed [`RESERVED_NAME_PREFIX`].
    ReservedName(String),
    /// A `builtin` field appeared in a manifest FILE. Provenance is stamped.
    BuiltinField(String),
    /// A scanned file used a field reserved for cImp's own embedded manifests.
    BuiltinOnlyField { tool: String, field: &'static str },
    /// A category references a tool id the manifest does not declare, or a tool
    /// is in no category / in two.
    Category(String),
    /// An argv token that is not `{root}` / `{report}` / `{var:NAME}`, a
    /// `{report}` without the `report_file` transport, or a `{var:NAME}`
    /// naming an undeclared variable.
    ArgvToken(String),
    /// A field that belongs to a different [`ToolKind`], or a mandatory one for
    /// this kind that is missing.
    KindField(String),
    /// A user plugin's audit/security tool named a parser other than `sarif`.
    ParserNotSarif { tool: String, found: String },
    /// `cwd`/`report_file` escapes the project root, or another
    /// [`CheckDef::validate`] rule fired.
    Confinement(String),
    /// An `extra_grants` entry that is not an absolute path.
    Grant(String),
    /// An `env` key that is not a plausible environment variable name.
    Env(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::Syntax(m) => write!(f, "not a valid manifest: {m}"),
            ValidationError::Size { bytes } => write!(
                f,
                "the manifest is {bytes} bytes, over the {MAX_MANIFEST_BYTES}-byte limit — a \
                 plugin definition is a short declaration, so a file this large is either not a \
                 manifest or not one this build should read"
            ),
            ValidationError::Timeout { tool, found } => write!(
                f,
                "tool `{tool}`: `timeout_secs` {found} is out of range (1..={MAX_TIMEOUT_SECS}) \
                 — 0 would mean both `no limit` and `kill it at once` depending on who read it, \
                 and anything past a day is a typo the pipeline would wait out"
            ),
            ValidationError::Version { found } => write!(
                f,
                "`manifest_version` {found} is not supported by this build (expected \
                 {MANIFEST_VERSION}) — the plugin was written for a different cImp"
            ),
            ValidationError::Identity(m) => write!(f, "identity: {m}"),
            ValidationError::ReservedName(n) => write!(
                f,
                "plugin name `{n}` uses the `{RESERVED_NAME_PREFIX}` prefix, which is reserved \
                 for cImp's own built-in plugins"
            ),
            ValidationError::BuiltinField(where_) => write!(
                f,
                "`builtin` is not a manifest field ({where_}): whether a plugin is built in is \
                 stamped by cImp when it loads the definition, never claimed by the file"
            ),
            ValidationError::BuiltinOnlyField { tool, field } => write!(
                f,
                "tool `{tool}` sets `{field}`, which only cImp's own built-in tool definitions \
                 may declare — each of those fields relaxes a rule (an output gate, a binary \
                 cImp resolves for you) that exists precisely because a dropped-in definition \
                 is not one cImp vouches for"
            ),
            ValidationError::Category(m) => write!(f, "categories: {m}"),
            ValidationError::ArgvToken(m) => write!(f, "argv: {m}"),
            ValidationError::KindField(m) => write!(f, "{m}"),
            ValidationError::ParserNotSarif { tool, found } => write!(
                f,
                "tool `{tool}`: an audit/security tool must deliver SARIF (`parser` must be \
                 `sarif`, found `{found}`) — cImp keeps one parse boundary for findings"
            ),
            ValidationError::Confinement(m) => write!(f, "{m}"),
            ValidationError::Grant(m) => write!(f, "extra_grants: {m}"),
            ValidationError::Env(m) => write!(f, "env: {m}"),
        }
    }
}

/// The id charset shared by plugin names, tool ids and category ids: safe as a
/// path component, as a namespace segment, and as an HTML attribute — the same
/// rule `theming::valid_id` applies to theme folders, and for the same reasons.
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A version string: the id charset plus `.` and `+` (semver build metadata),
/// and explicitly NOT `@` or `/`, which would make `name@version/tool` ambiguous.
fn valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

/// The length gate every identity and display string passes, in **characters**
/// rather than bytes so a manifest written in a non-Latin script is measured by
/// what a reader sees rather than by its UTF-8 weight.
///
/// A separate check from [`valid_id`]/[`valid_version`] on purpose: "not a
/// token" and "a token 4000 characters long" are different mistakes, and one
/// message that covered both would explain neither. `what` names the field so
/// the author knows which of a dozen strings to shorten.
fn length_checked(what: &str, value: &str) -> Result<(), ValidationError> {
    let len = value.chars().count();
    if len > MAX_NAME_CHARS {
        return Err(ValidationError::Identity(format!(
            "{what} is {len} characters, over the {MAX_NAME_CHARS}-character limit"
        )));
    }
    Ok(())
}

/// An env var name: non-empty, no `=`, no NUL, no whitespace.
///
/// Not an allow*list* — the allowlist of keys a child actually receives is the
/// spawn layer's (`sandbox::child_env`), and Phase G pins it. This is only the
/// parse-boundary shape check that keeps a malformed pair from reaching it.
fn valid_env_key(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('=')
        && !s.contains('\0')
        && !s.chars().any(char::is_whitespace)
}

/// Whether a path string is absolute in the **platform-agnostic** sense.
///
/// One verdict, shared with `checks::lexically_confined` via
/// [`crate::fsutil::looks_absolute`] — a manifest is a cross-platform artifact
/// (authored once, read on every OS), and a `CheckDef` is a shared config file,
/// so neither may get a different answer depending on which machine asks. The
/// real screening of a grant is V33's `extra_grant_refusal` at enable time, on
/// the machine whose paths these are; here we only refuse the shapes that are
/// *never* a grant: a relative path (which would silently resolve against
/// whatever cwd the spawn happened to have) and anything with a `..` component.
fn looks_absolute(s: &str) -> bool {
    crate::fsutil::looks_absolute(s)
}

/// One `{...}` group found in an argv token.
enum Token<'a> {
    Root,
    Report,
    Var(&'a str),
}

/// Scan an argv token for `{...}` groups, rejecting anything unrecognized.
///
/// `{{` is a literal `{` — an escape exists so that "a tool needs a real brace
/// in an argument" is expressible rather than a dead end that would push
/// authors toward disabling validation.
fn scan_tokens(arg: &str) -> Result<Vec<Token<'_>>, String> {
    let mut out = Vec::new();
    let bytes = arg.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'{') {
            i += 2; // escaped literal brace
            continue;
        }
        let Some(rel_end) = arg[i + 1..].find('}') else {
            return Err(format!(
                "`{arg}` has an unterminated `{{` (use `{{{{` for a literal brace)"
            ));
        };
        let inner = &arg[i + 1..i + 1 + rel_end];
        match inner {
            "root" => out.push(Token::Root),
            "report" => out.push(Token::Report),
            _ => match inner.strip_prefix("var:") {
                Some(name) if valid_id(name) => out.push(Token::Var(name)),
                _ => {
                    return Err(format!(
                        "`{arg}` uses unknown token `{{{inner}}}` (expected `{{root}}`, \
                         `{{report}}` or `{{var:NAME}}`)"
                    ));
                }
            },
        }
        i += 1 + rel_end + 1;
    }
    Ok(out)
}

/// Validate a parsed manifest under a load context.
///
/// `provenance` is the **loader's** stamp, and the two rules that key off it
/// (the reserved prefix, the SARIF requirement) are the reason it is a
/// parameter rather than a field: the same validator runs over the same shapes,
/// and only the trust level differs.
pub fn validate(m: &PluginManifest, provenance: Provenance) -> Result<(), ValidationError> {
    if m.builtin.is_some() {
        return Err(ValidationError::BuiltinField("plugin level".to_string()));
    }
    if m.manifest_version != MANIFEST_VERSION {
        return Err(ValidationError::Version {
            found: m.manifest_version,
        });
    }
    if !valid_id(&m.name) {
        return Err(ValidationError::Identity(format!(
            "`name` must be a non-empty [A-Za-z0-9_-] token (found `{}`)",
            m.name
        )));
    }
    length_checked("`name`", &m.name)?;
    if !valid_version(&m.version) {
        return Err(ValidationError::Identity(format!(
            "`version` must be a non-empty [A-Za-z0-9._+-] token (found `{}`)",
            m.version
        )));
    }
    length_checked("`version`", &m.version)?;
    // The plugin label is OPTIONAL (it falls back to `name`), but an empty one
    // is not the same as an absent one: absent means "use the name", present
    // and blank means the settings list renders a nameless row the user cannot
    // identify. Tool and category labels are already refused when blank; this
    // closes the one level that was not.
    if let Some(label) = &m.label {
        if label.trim().is_empty() {
            return Err(ValidationError::Identity(
                "plugin `label` is present but empty — omit it to fall back to `name`, or give \
                 it one; a blank label renders an unidentifiable row"
                    .to_string(),
            ));
        }
        length_checked("plugin `label`", label)?;
    }
    if provenance == Provenance::User && m.name.starts_with(RESERVED_NAME_PREFIX) {
        return Err(ValidationError::ReservedName(m.name.clone()));
    }
    // "Empty is not absent": a manifest declaring no tools is a well-formed
    // no-op that would sit in the settings list forever explaining nothing.
    // Refusing it here is cheaper than a UI state for "valid but does nothing".
    if m.tools.is_empty() {
        return Err(ValidationError::Identity(
            "a plugin must declare at least one tool".to_string(),
        ));
    }

    let mut seen_tools: BTreeSet<&str> = BTreeSet::new();
    for t in &m.tools {
        if !valid_id(&t.id) {
            return Err(ValidationError::Identity(format!(
                "tool `id` must be a non-empty [A-Za-z0-9_-] token (found `{}`)",
                t.id
            )));
        }
        if !seen_tools.insert(t.id.as_str()) {
            return Err(ValidationError::Identity(format!(
                "duplicate tool id `{}`",
                t.id
            )));
        }
        length_checked(&format!("tool id `{}`", t.id), &t.id)?;
        if t.label.trim().is_empty() {
            return Err(ValidationError::Identity(format!(
                "tool `{}` has an empty `label`",
                t.id
            )));
        }
        length_checked(&format!("tool `{}`'s `label`", t.id), &t.label)?;
        validate_tool(t, provenance)?;
    }

    // Categories: the management dimension must be a PARTITION of the tools.
    // A category toggle is a group operation (decision 9), so a tool in two
    // categories would have two conflicting group states, and a tool in none
    // would be unreachable in the settings pane that is its only control.
    let mut seen_cats: BTreeSet<&str> = BTreeSet::new();
    let mut owned: BTreeSet<&str> = BTreeSet::new();
    for c in &m.categories {
        if !valid_id(&c.id) {
            return Err(ValidationError::Category(format!(
                "category `id` must be a non-empty [A-Za-z0-9_-] token (found `{}`)",
                c.id
            )));
        }
        if !seen_cats.insert(c.id.as_str()) {
            return Err(ValidationError::Category(format!(
                "duplicate category id `{}`",
                c.id
            )));
        }
        length_checked(&format!("category id `{}`", c.id), &c.id)?;
        if c.label.trim().is_empty() {
            return Err(ValidationError::Category(format!(
                "category `{}` has an empty `label`",
                c.id
            )));
        }
        length_checked(&format!("category `{}`'s `label`", c.id), &c.label)?;
        for tid in &c.tools {
            if !seen_tools.contains(tid.as_str()) {
                return Err(ValidationError::Category(format!(
                    "category `{}` lists tool `{tid}`, which this plugin does not declare",
                    c.id
                )));
            }
            if !owned.insert(tid.as_str()) {
                return Err(ValidationError::Category(format!(
                    "tool `{tid}` appears in more than one category — a category toggle is a \
                     group operation, so a tool may belong to exactly one"
                )));
            }
        }
    }
    for t in &m.tools {
        if !owned.contains(t.id.as_str()) {
            return Err(ValidationError::Category(format!(
                "tool `{}` belongs to no category, so nothing in settings could enable it",
                t.id
            )));
        }
    }
    Ok(())
}

/// The per-tool half of [`validate`]. Split out because the kind cross-checks
/// are the bulk of the rules and read better as a table of "this field belongs
/// to that kind".
fn validate_tool(t: &ToolManifest, provenance: Provenance) -> Result<(), ValidationError> {
    if t.builtin.is_some() {
        return Err(ValidationError::BuiltinField(format!("tool `{}`", t.id)));
    }

    // The built-in-only fields, refused on the SCANNED path by the same rule
    // that refuses `builtin`: the loader's stamp, never a name string. Checked
    // first, so a file trying to relax the output gate is told that rather than
    // being told about some later field it also got wrong.
    if provenance == Provenance::User {
        let reserved: [(&'static str, bool); 4] = [
            ("ingest", t.ingest.is_some()),
            ("command", t.command.is_some()),
            ("project_local_bin", t.project_local_bin.is_some()),
            ("dir_argv", !t.dir_argv.is_empty()),
        ];
        for (field, present) in reserved {
            if present {
                return Err(ValidationError::BuiltinOnlyField {
                    tool: t.id.clone(),
                    field,
                });
            }
        }
    }

    for (k, _v) in &t.env {
        if !valid_env_key(k) {
            return Err(ValidationError::Env(format!(
                "tool `{}` forces an env variable whose name is not usable (`{k}`)",
                t.id
            )));
        }
    }
    for g in &t.extra_grants {
        if !looks_absolute(g) {
            return Err(ValidationError::Grant(format!(
                "tool `{}` requests `{g}`, which is not an absolute path — a relative grant \
                 would resolve against whatever directory the spawn happened to have",
                t.id
            )));
        }
        if std::path::Path::new(g)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
            || g.split(['/', '\\']).any(|seg| seg == "..")
        {
            return Err(ValidationError::Grant(format!(
                "tool `{}` requests `{g}`, which contains a `..` component",
                t.id
            )));
        }
    }

    // A timeout is a wall-clock budget a pipeline waits out, so both ends are
    // bounded — see [`MAX_TIMEOUT_SECS`] for why 0 is refused rather than
    // interpreted.
    if let Some(secs) = t.timeout_secs {
        if secs == 0 || secs > MAX_TIMEOUT_SECS {
            return Err(ValidationError::Timeout {
                tool: t.id.clone(),
                found: secs,
            });
        }
    }

    // Built by INSERTION, not `collect`: a set collected from the list silently
    // swallows a duplicate `name`, and the author of `[{ruleset, default "a"},
    // {ruleset, default "b"}]` gets one field in the settings pane with one of
    // the two defaults, chosen by list order — a value they never wrote and
    // cannot see is wrong. Two declarations of one variable is a mistake with
    // no correct reading, so it is refused.
    let mut declared_vars: BTreeSet<&str> = BTreeSet::new();
    for v in &t.variables {
        if !valid_id(&v.name) {
            return Err(ValidationError::Identity(format!(
                "tool `{}` declares variable `{}`, which is not a [A-Za-z0-9_-] token",
                t.id, v.name
            )));
        }
        length_checked(&format!("tool `{}`'s variable `{}`", t.id, v.name), &v.name)?;
        if v.label.trim().is_empty() {
            return Err(ValidationError::Identity(format!(
                "tool `{}`'s variable `{}` has an empty `label` — it is what the settings pane \
                 puts beside the input",
                t.id, v.name
            )));
        }
        length_checked(
            &format!("tool `{}`'s variable `{}` label", t.id, v.name),
            &v.label,
        )?;
        if !declared_vars.insert(v.name.as_str()) {
            return Err(ValidationError::Identity(format!(
                "tool `{}` declares the variable `{}` twice — one input cannot carry two \
                 declarations, and `{{var:{}}}` could not say which it meant",
                t.id, v.name, v.name
            )));
        }
    }

    // A resolved command is a NAME, not a path: `pty::resolve` searches `ebin`
    // then `PATH` for it, and a value carrying a separator would silently
    // become a relative path against whatever directory the spawn had. Refused
    // rather than normalized, because "which binary ran" is not a question to
    // answer by guessing.
    for (field, value) in [
        ("command", t.command.as_deref()),
        ("project_local_bin", t.project_local_bin.as_deref()),
    ] {
        let Some(value) = value else { continue };
        if value.trim().is_empty()
            || value.contains(['/', '\\'])
            || looks_absolute(value)
        {
            return Err(ValidationError::Identity(format!(
                "tool `{}`: `{field}` (`{value}`) must be a bare command NAME — cImp searches \
                 `ebin` and then `PATH` for it, and a value carrying a path separator would \
                 resolve against whatever directory the spawn happened to have",
                t.id
            )));
        }
        length_checked(&format!("tool `{}`'s `{field}`", t.id), value)?;
    }

    // Fields that belong to a kind other than this one. Refused rather than
    // ignored: a `cmd` on an audit tool is an author who believes something
    // will run that never will.
    let wrong = |field: &str, belongs: &str| {
        ValidationError::KindField(format!(
            "tool `{}` is a `{}` tool but sets `{field}`, which is a {belongs}-kind field",
            t.id,
            t.kind.as_str()
        ))
    };

    match t.kind {
        ToolKind::Audit | ToolKind::Security => {
            if t.cmd.is_some() {
                return Err(wrong("cmd", "check"));
            }
            if t.cwd.is_some() {
                return Err(wrong("cwd", "check"));
            }
            if t.report_file.is_some() {
                return Err(wrong("report_file", "check"));
            }
            if t.pattern.is_some() {
                return Err(wrong("pattern", "check"));
            }
            // A findings tool with no fixed arguments at all is almost always
            // an author who forgot the template, so a scanned file is refused.
            // It is NOT always that: `cargo-machete` analyses its cwd and takes
            // no arguments, and cImp has shipped it since V25 — so the built-in
            // tier, whose fourteen shapes were measured against real binaries,
            // is allowed to say "none" and mean it.
            if t.argv.is_empty() && provenance == Provenance::User {
                return Err(ValidationError::KindField(format!(
                    "tool `{}` is a `{}` tool and needs an `argv` template",
                    t.id,
                    t.kind.as_str()
                )));
            }

            let parser = t.parser.unwrap_or(ManifestParser::SARIF);
            // R2/R3: the legacy per-tool parsers exist for the embedded set
            // only. A user plugin's findings must enter through the one SARIF
            // boundary — enforced here, not documented.
            if provenance == Provenance::User && parser != ManifestParser::SARIF {
                return Err(ValidationError::ParserNotSarif {
                    tool: t.id.clone(),
                    found: parser.as_wire(),
                });
            }

            let transport = t.transport.unwrap_or_default();
            let mut saw_report = false;
            // `dir_argv` is scanned with `argv` and under the same rules: it is
            // a template this tool can be spawned with, so "it is only the
            // fallback" is not a reason to check it less.
            for arg in t.argv.iter().chain(t.dir_argv.iter()) {
                for tok in scan_tokens(arg).map_err(ValidationError::ArgvToken)? {
                    match tok {
                        Token::Root => {}
                        Token::Report => saw_report = true,
                        Token::Var(name) => {
                            if !declared_vars.contains(name) {
                                return Err(ValidationError::ArgvToken(format!(
                                    "tool `{}` uses `{{var:{name}}}`, which it does not declare \
                                     in `variables`",
                                    t.id
                                )));
                            }
                        }
                    }
                }
            }
            if saw_report && transport != Transport::ReportFile {
                return Err(ValidationError::ArgvToken(format!(
                    "tool `{}` uses `{{report}}` but its `transport` is `stdout` — there is no \
                     report path to substitute, so the token would render empty",
                    t.id
                )));
            }
            if !saw_report && transport == Transport::ReportFile {
                return Err(ValidationError::ArgvToken(format!(
                    "tool `{}` declares the `report_file` transport but its `argv` never uses \
                     `{{report}}`, so cImp would read a file the tool was never told to write",
                    t.id
                )));
            }
        }
        ToolKind::Check => {
            if !t.argv.is_empty() {
                return Err(wrong("argv", "audit/security"));
            }
            if !t.dir_argv.is_empty() {
                return Err(wrong("dir_argv", "audit/security"));
            }
            if t.ingest.is_some() {
                return Err(wrong("ingest", "audit/security"));
            }
            if t.transport.is_some() {
                return Err(wrong("transport", "audit/security"));
            }
            if !t.findings_exit_codes.is_empty() {
                return Err(wrong("findings_exit_codes", "audit/security"));
            }
            let cmd = t.cmd.as_deref().unwrap_or("");
            if cmd.trim().is_empty() {
                return Err(ValidationError::KindField(format!(
                    "tool `{}` is a `check` tool and needs a `cmd`",
                    t.id
                )));
            }
            // The command line must BEGIN with a plain program token, because
            // that token is the placeholder cImp replaces with the binary the
            // user configured (decision 7: a manifest never names an
            // executable). A line starting with an `ENV=value` prefix, a
            // pipeline, a redirection or a quoting oddity has nowhere to put
            // the path — and refusing it HERE rather than at the first
            // `run_check` call is the one-artifact-one-verdict rule this arm
            // already applies to `cwd`/`report_file`: a plugin is authored once
            // and read on every machine, so it must not load on one and fail
            // mid-session on the next.
            if crate::checks::split_first_shell_token(cmd).is_none() {
                return Err(ValidationError::KindField(format!(
                    "tool `{}`: `cmd` (`{cmd}`) must begin with a plain program name — cImp \
                     replaces that first token with the binary path the user configures for this \
                     tool, and a command line that starts with anything else (an `NAME=value` \
                     prefix, a pipeline, a redirection) gives it nowhere to go",
                    t.id
                )));
            }
            // `{var:NAME}` substitution applies to a check's command line too —
            // same token vocabulary, same undeclared-variable rule. `{report}`
            // has no meaning here (a check names its report file directly).
            for tok in scan_tokens(cmd).map_err(ValidationError::ArgvToken)? {
                match tok {
                    Token::Var(name) if declared_vars.contains(name) => {}
                    Token::Var(name) => {
                        return Err(ValidationError::ArgvToken(format!(
                            "tool `{}` uses `{{var:{name}}}`, which it does not declare in \
                             `variables`",
                            t.id
                        )));
                    }
                    Token::Root => {}
                    Token::Report => {
                        return Err(ValidationError::ArgvToken(format!(
                            "tool `{}` is a `check` tool and cannot use `{{report}}` — name the \
                             file in `report_file` instead",
                            t.id
                        )));
                    }
                }
            }
            let parser = match t.parser.unwrap_or(ManifestParser::Kind(ParserKind::default())) {
                ManifestParser::Kind(k) => k,
                ManifestParser::Legacy(l) => {
                    return Err(ValidationError::KindField(format!(
                        "tool `{}` is a `check` tool and cannot use the audit-only parser `{}`",
                        t.id,
                        l.as_str()
                    )));
                }
            };
            // The path-shaped fields must be absolute-free in the SAME
            // cross-platform sense `extra_grants` uses, and that check has to
            // happen here rather than only in `CheckDef::validate`.
            //
            // **Discovered building this** (reported, not worked around):
            // `checks::lexically_confined` tests `Path::is_absolute()`, which is
            // evaluated for the RUNNING platform — so `"/etc"` is not lexically
            // absolute on Windows and passes. For a `CheckDef` that is only a
            // latent gap, because `confine_under_root` re-checks canonically at
            // spawn time (a root joined with "/etc" resolves to `C:\etc` on
            // Windows, which then fails that boundary). For a MANIFEST it would be
            // worse: a plugin is authored once and read on every platform, so the
            // file refused on Linux would load on Windows and be refused later, at
            // run time, by a different message. One artifact, one verdict.
            if let Some(rel) = t.cwd.as_deref().filter(|s| !s.is_empty()) {
                if looks_absolute(rel) {
                    return Err(ValidationError::Confinement(format!(
                        "tool `{}`: `cwd` `{rel}` must be relative to the project root",
                        t.id
                    )));
                }
            }
            if let Some(rel) = t.report_file.as_deref().filter(|s| !s.is_empty()) {
                if looks_absolute(rel) {
                    return Err(ValidationError::Confinement(format!(
                        "tool `{}`: `report_file` `{rel}` must be relative to the project root",
                        t.id
                    )));
                }
            }
            // For everything else, reuse `CheckDef`'s own validation rather
            // than re-deriving it: the `..` confinement AND the
            // `regex-custom`-needs-a-pattern rule are contracts that already
            // exist, and a second copy is a second thing to keep in step.
            let probe = CheckDef {
                name: t.id.clone(),
                cmd: cmd.to_string(),
                parser,
                cwd: t.cwd.clone(),
                report_file: t.report_file.clone(),
                pattern: t.pattern.clone(),
                ..CheckDef::default()
            };
            probe.validate().map_err(|e| {
                ValidationError::Confinement(format!("tool `{}`: {e}", t.id))
            })?;
        }
        ToolKind::Command => {
            // A command-kind entry is *identity only*: the path and the enable
            // come from user state (decision 10), and the arguments come from
            // the caller of `run_command`. Anything else declared here would be
            // a template nothing renders.
            //
            // **The three fields below are refused rather than ignored** — a
            // tightening ordered after the Phase G spec pass found them accepted
            // by the schema and read by nothing. Each has its own reason, and
            // none of them is tidiness:
            //
            // * `timeout_secs` — `run_command` runs every tool under ONE fixed
            //   budget because it is advertised to a model as a short read-only
            //   probe. Honouring a manifest's timeout would quietly turn it into
            //   a long-job runner, and the description a model reads would stop
            //   being true.
            // * `env` — a manifest-supplied environment is grant-shaped power
            //   (`LD_PRELOAD`, `PYTHONPATH`, a proxy pointer) with no
            //   enable-time disclosure: unlike `extra_grants`, nothing shows it
            //   to the user beside the switch that turns the tool on.
            // * `variables` — this kind has no template to substitute a value
            //   into, so a declaration would render a settings input whose value
            //   went nowhere. That is worse than not offering the field.
            //
            // Refusing is reversible — a later phase can consume any of them and
            // every manifest written today keeps loading. Consuming is not: a
            // manifest authored against a field that silently did nothing would
            // change behaviour the day it started working.
            let unread = |field: &str, why: &str| {
                ValidationError::KindField(format!(
                    "tool `{}` is a `command` tool and sets `{field}`, which nothing reads on \
                     this kind — {why}",
                    t.id
                ))
            };
            if t.timeout_secs.is_some() {
                return Err(unread(
                    "timeout_secs",
                    "`run_command` runs every tool under one fixed budget, because it is \
                     advertised to a model as a short read-only probe",
                ));
            }
            if !t.env.is_empty() {
                return Err(unread(
                    "env",
                    "`run_command` composes its child environment from cImp's allowlist plus the \
                     applicable command policy, and a manifest-set variable would be \
                     grant-shaped power with nothing disclosing it at enable time",
                ));
            }
            if !t.variables.is_empty() {
                return Err(unread(
                    "variables",
                    "there is no argv or command template on this kind to substitute a value \
                     into, so the settings pane would render an input that goes nowhere",
                ));
            }
            // V38 Phase F, the same tightening two fields further. Both were
            // found by the plugin-pack pass: they validated and nothing on this
            // kind read them.
            //
            // * `parameters_allowed` - `run_command` takes its arguments from
            //   the CALLER (the model), so a stored "extra CLI parameters" value
            //   has no argv to be appended to. The settings pane would render an
            //   input whose contents never reach a command line.
            // * `applicability` - a project-shape gate filters a POPULATION. The
            //   audit umbrellas filter their fan-out and `run_check` filters its
            //   advertised set; `run_command` has neither - it resolves one named
            //   tool on demand. A gate here would be a promise ("this tool only
            //   exists in a Maven project") that nothing keeps.
            //
            // `parameters_allowed` is refused only when TRUE: it is a plain
            // `bool`, so an explicit `false` cannot be told apart from an absent
            // field, and refusing the pair would refuse manifests that say
            // exactly what this rule wants them to say.
            if t.parameters_allowed {
                return Err(unread(
                    "parameters_allowed",
                    "`run_command` takes its arguments from the caller, so there is no argv \
                     for a stored parameter list to be appended to",
                ));
            }
            if !t.applicability.extensions.is_empty() || !t.applicability.markers.is_empty() {
                return Err(unread(
                    "applicability",
                    "a project-shape gate filters a population, and `run_command` has none \
                     — it resolves one named tool on demand rather than fanning out over a set",
                ));
            }
            if !t.argv.is_empty() {
                return Err(wrong("argv", "audit/security"));
            }
            if !t.dir_argv.is_empty() {
                return Err(wrong("dir_argv", "audit/security"));
            }
            if t.ingest.is_some() {
                return Err(wrong("ingest", "audit/security"));
            }
            if t.transport.is_some() {
                return Err(wrong("transport", "audit/security"));
            }
            if !t.findings_exit_codes.is_empty() {
                return Err(wrong("findings_exit_codes", "audit/security"));
            }
            if t.parser.is_some() {
                return Err(ValidationError::KindField(format!(
                    "tool `{}` is a `command` tool: its output is raw, so it has no `parser`",
                    t.id
                )));
            }
            if t.cmd.is_some() {
                return Err(wrong("cmd", "check"));
            }
            if t.cwd.is_some() {
                return Err(wrong("cwd", "check"));
            }
            if t.report_file.is_some() {
                return Err(wrong("report_file", "check"));
            }
            if t.pattern.is_some() {
                return Err(wrong("pattern", "check"));
            }
        }
    }
    Ok(())
}

/// Why one manifest did not load, **with the identity it managed to state**.
///
/// The identity is carried out of here rather than recovered by the caller: a
/// file that failed *validation* has already been deserialized once, so its
/// `name`/`version` are sitting right there, and re-parsing the whole text as a
/// `Value` to fish them back out is a second full parse of the exact input we
/// just decided not to trust. A file that failed to PARSE has no trustworthy
/// identity at all and gets `None` — the settings pane then shows the file
/// name, which is the honest answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseFailure {
    pub error: ValidationError,
    /// `name@version`, when the file got far enough to have one.
    pub key: Option<String>,
}

/// Displays as the reason alone. The identity is a separate column everywhere
/// it is shown (the settings row's title, the Events row's `source`), so
/// folding it into the sentence would print it twice.
impl std::fmt::Display for ParseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// Parse + validate one manifest's bytes. The single entry point the loader
/// uses, so "parsed" and "validated" cannot come apart.
pub fn parse(text: &str, provenance: Provenance) -> Result<PluginManifest, ParseFailure> {
    // The contract half of the size cap (the loader enforces the resource half
    // before it reads a byte). Here so the embedded built-in path Phase E adds
    // passes the same boundary as a scanned file — one validator, one limit.
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ParseFailure {
            error: ValidationError::Size {
                bytes: text.len() as u64,
            },
            key: None,
        });
    }
    let m: PluginManifest = serde_json::from_str(text).map_err(|e| ParseFailure {
        error: ValidationError::Syntax(e.to_string()),
        key: None,
    })?;
    validate(&m, provenance).map_err(|error| ParseFailure {
        error,
        key: Some(m.key()),
    })?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal well-formed manifest, as text, so the tests exercise the real
    /// parse path (serde's `deny_unknown_fields` included) rather than a
    /// hand-built struct that can never carry an unknown key.
    fn audit_json() -> String {
        r#"{
          "manifest_version": 1,
          "name": "acme",
          "version": "1.0.0",
          "categories": [{ "id": "sec", "label": "Security", "tools": ["scan"] }],
          "tools": [{
            "id": "scan", "label": "Acme Scan", "kind": "security",
            "argv": ["--sarif", "--rules", "{var:ruleset}", "{root}"],
            "variables": [{ "name": "ruleset", "label": "Ruleset", "default": "p/default" }],
            "findings_exit_codes": [1],
            "applicability": { "extensions": ["rs"], "markers": [] }
          }]
        }"#
        .to_string()
    }

    fn check_json() -> String {
        r#"{
          "manifest_version": 1,
          "name": "acme",
          "version": "1.0.0",
          "categories": [{ "id": "build", "label": "Build", "tools": ["t"] }],
          "tools": [{
            "id": "t", "label": "Acme Build", "kind": "check",
            "cmd": "acme build --json", "parser": "generic-gcc",
            "cwd": "sub", "report_file": "sub/out.txt", "timeout_secs": 300
          }]
        }"#
        .to_string()
    }

    fn command_json() -> String {
        r#"{
          "manifest_version": 1,
          "name": "acme",
          "version": "1.0.0",
          "categories": [{ "id": "vcs", "label": "Source control", "tools": ["git"] }],
          "tools": [{ "id": "git", "label": "git", "kind": "command", "runtime": "none" }]
        }"#
        .to_string()
    }

    fn err(json: &str) -> ValidationError {
        parse(json, Provenance::User)
            .expect_err("manifest should have been rejected")
            .error
    }

    #[test]
    fn a_well_formed_manifest_of_each_kind_loads() {
        for (what, json) in [
            ("audit/security", audit_json()),
            ("check", check_json()),
            ("command", command_json()),
        ] {
            let m = parse(&json, Provenance::User)
                .unwrap_or_else(|e| panic!("{what} manifest rejected: {e}"));
            assert_eq!(m.key(), "acme@1.0.0");
            // The namespacing itself is `LoadedPlugin::tool_key`'s (the loader
            // owns identity); here we only pin the half the manifest supplies.
            assert_eq!(m.tools.len(), 1);
        }
    }

    /// The defaults are the SAFE ones — an author who thinks about none of this
    /// gets a sandboxed tool with inferred grants, not an unconfined one.
    #[test]
    fn omitted_sandbox_fields_default_to_the_safe_posture() {
        let m = parse(&command_json(), Provenance::User).expect("valid");
        let t = &m.tools[0];
        assert_eq!(t.sandbox, SandboxReq::Required);
        assert!(!t.parameters_allowed);
        assert!(t.extra_grants.is_empty());
        // …and `runtime` defaults to inference when the file is silent.
        let m2 = parse(&audit_json(), Provenance::User).expect("valid");
        assert_eq!(m2.tools[0].runtime, RuntimeReq::Auto);
    }

    #[test]
    fn an_unsupported_manifest_version_is_a_load_error() {
        let json = audit_json().replace("\"manifest_version\": 1", "\"manifest_version\": 2");
        assert_eq!(err(&json), ValidationError::Version { found: 2 });
    }

    #[test]
    fn identity_is_mandatory_in_both_halves() {
        for (field, replacement) in [
            ("\"name\": \"acme\"", "\"name\": \"\""),
            ("\"version\": \"1.0.0\"", "\"version\": \"\""),
        ] {
            let json = audit_json().replace(field, replacement);
            assert!(
                matches!(err(&json), ValidationError::Identity(_)),
                "{field} empty must be an identity error"
            );
        }
        // Missing entirely (not just empty) is a syntax error from serde —
        // still a refusal, which is the property that matters.
        let json = audit_json().replace("\"version\": \"1.0.0\",", "");
        assert!(matches!(err(&json), ValidationError::Syntax(_)));
    }

    #[test]
    fn an_undeclared_variable_token_is_rejected() {
        let json = audit_json().replace("{var:ruleset}", "{var:nope}");
        match err(&json) {
            ValidationError::ArgvToken(m) => assert!(m.contains("nope"), "{m}"),
            other => panic!("expected an argv-token error, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_argv_token_is_rejected() {
        let json = audit_json().replace("{root}", "{home}");
        assert!(matches!(err(&json), ValidationError::ArgvToken(_)));
    }

    /// `{{` is the escape, so a real brace in an argument is expressible.
    #[test]
    fn an_escaped_brace_is_not_a_token() {
        let json = audit_json().replace("\"{root}\"", "\"{{literal}} {root}\"");
        parse(&json, Provenance::User).expect("an escaped brace is a literal, not a token");
    }

    #[test]
    fn a_report_token_without_the_report_file_transport_is_rejected() {
        let json = audit_json().replace("\"{root}\"", "\"{report}\", \"{root}\"");
        match err(&json) {
            ValidationError::ArgvToken(m) => assert!(m.contains("report"), "{m}"),
            other => panic!("expected an argv-token error, got {other:?}"),
        }
        // …and declaring the transport makes the same manifest valid.
        let ok = json.replace(
            "\"kind\": \"security\",",
            "\"kind\": \"security\", \"transport\": \"report_file\",",
        );
        parse(&ok, Provenance::User).expect("`{report}` + report_file transport is the valid pair");
    }

    /// The other direction: a declared report file that argv never names would
    /// have cImp read a file nothing was asked to write.
    #[test]
    fn a_report_file_transport_without_the_token_is_rejected() {
        let json = audit_json().replace(
            "\"kind\": \"security\",",
            "\"kind\": \"security\", \"transport\": \"report_file\",",
        );
        assert!(matches!(err(&json), ValidationError::ArgvToken(_)));
    }

    /// R2/R3, the gate that must key off PROVENANCE and nothing else.
    #[test]
    fn a_user_plugins_findings_tool_must_be_sarif_but_a_builtin_may_not_be() {
        let json = audit_json().replace(
            "\"kind\": \"security\",",
            "\"kind\": \"security\", \"parser\": \"typos-jsonl\",",
        );
        match err(&json) {
            ValidationError::ParserNotSarif { tool, found } => {
                assert_eq!(tool, "scan");
                assert_eq!(found, "typos-jsonl");
            }
            other => panic!("expected ParserNotSarif, got {other:?}"),
        }
        // The SAME bytes load on the embedded path Phase E will use.
        parse(&json, Provenance::Builtin)
            .expect("a legacy parser is exactly what the built-in path exists for");
        // And an explicit `sarif` is fine either way.
        let sarif = audit_json().replace(
            "\"kind\": \"security\",",
            "\"kind\": \"security\", \"parser\": \"sarif\",",
        );
        parse(&sarif, Provenance::User).expect("sarif is the user-plugin contract");
    }

    /// Provenance is stamped by the loader; a file that claims it is forging
    /// the flag the security floor and the parser gate both key off.
    #[test]
    fn a_builtin_field_is_rejected_at_either_level() {
        let plugin_level = audit_json().replace(
            "\"manifest_version\": 1,",
            "\"manifest_version\": 1, \"builtin\": true,",
        );
        assert!(matches!(
            err(&plugin_level),
            ValidationError::BuiltinField(_)
        ));
        let tool_level = audit_json().replace(
            "\"id\": \"scan\",",
            "\"id\": \"scan\", \"builtin\": true,",
        );
        assert!(matches!(err(&tool_level), ValidationError::BuiltinField(_)));
        // Even on the embedded path: provenance is the loader's, always.
        assert!(matches!(
            parse(&plugin_level, Provenance::Builtin),
            Err(ParseFailure {
                error: ValidationError::BuiltinField(_),
                ..
            })
        ));
    }

    #[test]
    fn the_cimp_prefix_is_reserved_for_our_own_plugins() {
        let json = audit_json().replace("\"name\": \"acme\"", "\"name\": \"cimp-acme\"");
        assert!(matches!(err(&json), ValidationError::ReservedName(_)));
        // Reserved *for* built-ins, so the embedded path accepts it.
        parse(&json, Provenance::Builtin).expect("the prefix is reserved for us, not from us");
    }

    #[test]
    fn a_check_tool_cannot_escape_the_project_root() {
        for (from, to) in [
            // `..` out of the root, on either path-shaped field…
            ("\"cwd\": \"sub\"", "\"cwd\": \"../out\""),
            ("\"report_file\": \"sub/out.txt\"", "\"report_file\": \"../out.txt\""),
            // …and the absolute form of the same escape, in BOTH platforms'
            // shapes: a manifest is authored once and read everywhere.
            ("\"cwd\": \"sub\"", "\"cwd\": \"/etc\""),
            ("\"cwd\": \"sub\"", "\"cwd\": \"C:\\\\Windows\""),
            ("\"report_file\": \"sub/out.txt\"", "\"report_file\": \"/tmp/out\""),
        ] {
            let json = check_json().replace(from, to);
            assert!(
                matches!(err(&json), ValidationError::Confinement(_)),
                "{to} should have been refused"
            );
        }
    }

    /// A timeout is a wall-clock budget something waits out, so both ends are
    /// bounded — and 0 is refused rather than silently meaning one of the two
    /// opposite things a reader would take it for.
    ///
    /// Exercised on an AUDIT tool: a `command`-kind tool refuses `timeout_secs`
    /// outright (nothing on that kind reads one), so the range rule has no
    /// values to have an opinion about there.
    #[test]
    fn a_timeout_outside_the_supported_range_is_refused() {
        let with_timeout = |secs: u64| {
            audit_json().replace(
                r#""id": "scan","#,
                &format!(r#""id": "scan", "timeout_secs": {secs},"#),
            )
        };
        for bad in [0u64, MAX_TIMEOUT_SECS + 1, u64::MAX] {
            match err(&with_timeout(bad)) {
                ValidationError::Timeout { tool, found } => {
                    assert_eq!(tool, "scan");
                    assert_eq!(found, bad);
                }
                other => panic!("expected a timeout error for {bad}, got {other:?}"),
            }
        }
        for ok in [1u64, 600, MAX_TIMEOUT_SECS] {
            parse(&with_timeout(ok), Provenance::User)
                .unwrap_or_else(|e| panic!("{ok}s rejected: {e:?}"));
        }
    }

    /// The size cap is a *contract*, not only the loader's resource guard: the
    /// same limit has to hold on the embedded built-in path Phase E adds, which
    /// never touches a file at all.
    #[test]
    fn a_manifest_over_the_size_cap_is_refused_by_bytes() {
        // Well-formed and enormous: the refusal must be about the size, not
        // about the shape, or the message would send the author hunting a
        // syntax error that isn't there.
        let padding = "x".repeat(MAX_MANIFEST_BYTES as usize);
        let json = audit_json().replace(
            "\"version\": \"1.0.0\"",
            &format!("\"version\": \"1.0.0\", \"description\": \"{padding}\""),
        );
        match err(&json) {
            ValidationError::Size { bytes } => {
                assert!(bytes > MAX_MANIFEST_BYTES);
                let msg = ValidationError::Size { bytes }.to_string();
                assert!(msg.contains(&bytes.to_string()), "{msg}");
            }
            other => panic!("expected a size error, got {other:?}"),
        }
    }

    /// Every identity and display string is bounded. A 4000-character "label"
    /// is a payload wearing a name's clothes, and every renderer downstream
    /// would have to invent its own truncation.
    #[test]
    fn an_over_long_identity_or_label_is_refused() {
        let long = "a".repeat(MAX_NAME_CHARS + 1);
        let cases = [
            audit_json().replace("\"name\": \"acme\"", &format!("\"name\": \"{long}\"")),
            audit_json().replace("\"version\": \"1.0.0\"", &format!("\"version\": \"{long}\"")),
            audit_json().replace("\"id\": \"sec\"", &format!("\"id\": \"{long}\"")),
            audit_json().replace("\"label\": \"Security\"", &format!("\"label\": \"{long}\"")),
            audit_json().replace("\"id\": \"scan\"", &format!("\"id\": \"{long}\"")),
            audit_json().replace("\"label\": \"Acme Scan\"", &format!("\"label\": \"{long}\"")),
            audit_json().replace(
                "\"name\": \"acme\",",
                &format!("\"name\": \"acme\", \"label\": \"{long}\","),
            ),
        ];
        for (i, json) in cases.iter().enumerate() {
            let e = err(json);
            assert!(
                matches!(e, ValidationError::Identity(_) | ValidationError::Category(_)),
                "case {i}: expected a length refusal, got {e:?}"
            );
            assert!(
                e.to_string().contains("characters"),
                "case {i}: the message must say what is wrong: {e}"
            );
        }
        // The boundary itself is allowed — the cap is a limit, not an off-by-one.
        let at_limit = "a".repeat(MAX_NAME_CHARS);
        let json = audit_json().replace("\"label\": \"Acme Scan\"", &format!("\"label\": \"{at_limit}\""));
        parse(&json, Provenance::User).expect("exactly at the limit is fine");
    }

    /// `C:foo` is **drive-relative** — "foo, under whatever the current
    /// directory on drive C happens to be" — so granting it would hand a
    /// sandboxed child a directory chosen by per-drive process state.
    #[test]
    fn a_drive_relative_grant_is_not_an_absolute_path() {
        for bad in ["C:foo", "C:", "c:tools\\\\acme"] {
            let json = audit_json().replace(
                "\"id\": \"scan\",",
                &format!("\"id\": \"scan\", \"extra_grants\": [\"{bad}\"],"),
            );
            assert!(
                matches!(err(&json), ValidationError::Grant(_)),
                "`{bad}` is drive-relative and must not pass as an absolute grant"
            );
        }
    }

    /// Two declarations of one variable have no correct reading: the settings
    /// pane renders one input, and which default it carries would be decided by
    /// list order — a value the author never wrote.
    #[test]
    fn a_variable_declared_twice_is_refused_not_collapsed() {
        let json = audit_json().replace(
            "\"variables\": [{ \"name\": \"ruleset\", \"label\": \"Ruleset\", \"default\": \"p/default\" }]",
            "\"variables\": [{ \"name\": \"ruleset\", \"label\": \"A\", \"default\": \"a\" }, \
             { \"name\": \"ruleset\", \"label\": \"B\", \"default\": \"b\" }]",
        );
        match err(&json) {
            ValidationError::Identity(m) => assert!(m.contains("twice"), "{m}"),
            other => panic!("expected a duplicate-variable refusal, got {other:?}"),
        }
    }

    /// A present-but-blank plugin label is not the same as an absent one:
    /// absent falls back to `name`, blank renders a row nobody can identify.
    /// "Empty is not absent", at the one level that still allowed it.
    #[test]
    fn a_present_but_empty_plugin_label_is_refused_while_an_absent_one_is_fine() {
        for blank in ["", "   "] {
            let json = audit_json().replace(
                "\"name\": \"acme\",",
                &format!("\"name\": \"acme\", \"label\": \"{blank}\","),
            );
            match err(&json) {
                ValidationError::Identity(m) => assert!(m.contains("label"), "{m}"),
                other => panic!("expected an empty-label refusal, got {other:?}"),
            }
        }
        // Absent is the normal case and stays valid.
        parse(&audit_json(), Provenance::User).expect("no label at all is fine");
    }

    /// **Lockstep with `checks`.** The manifest and `CheckDef::validate` must
    /// reach ONE verdict on "is this absolute?", on every platform — they share
    /// `fsutil::looks_absolute` precisely so they cannot drift, and this asserts
    /// the agreement rather than trusting it. The twin lives at
    /// `checks::tests::validate_rejects_the_other_platforms_absolute_form_too`;
    /// tighten the predicate and both move.
    #[test]
    fn both_platforms_absolute_forms_are_refused() {
        for abs in ["/etc", "C:\\Windows", "\\\\srv\\share", "c:/tools"] {
            assert!(looks_absolute(abs), "`{abs}` must read as absolute here");
            let probe = CheckDef {
                cwd: Some(abs.to_string()),
                ..CheckDef::default()
            };
            assert!(
                probe.validate().is_err(),
                "`{abs}` is absolute to the manifest but not to CheckDef — the two \
                 verdicts have drifted apart"
            );
        }
    }

    /// V38 Phase D: a check's `cmd` must BEGIN with a plain program token,
    /// because that token is the placeholder cImp replaces with the binary the
    /// user configured (decision 7 — a manifest never names an executable).
    /// Refused at LOAD, not at the first `run_check`: a plugin is authored once
    /// and read on every machine, so one artifact gets one verdict.
    #[test]
    fn a_check_cmd_must_begin_with_a_program_token() {
        // Swap the `cmd` VALUE, quotes and all, through serde's own escaper —
        // a Windows path in a JSON string is backslashes, and hand-escaping it
        // in a test fixture is how a test ends up asserting on a parse error.
        let with_cmd = |cmd: &str| {
            check_json().replace(
                "\"acme build --json\"",
                &serde_json::to_string(cmd).expect("escape"),
            )
        };
        for bad in [
            "FOO=bar acme build",   // an sh variable prefix — the program is the NEXT token
            "| acme build",         // a pipeline
            "> out.txt acme build", // a redirection
            "(acme build)",         // a subshell
            "\"unterminated acme",  // a quote with no partner
        ] {
            match err(&with_cmd(bad)) {
                ValidationError::KindField(m) => {
                    assert!(m.contains("plain program name"), "{bad}: {m}")
                }
                other => panic!("`{bad}` must be refused as a KindField, got {other:?}"),
            }
        }
        // The shapes that DO name a program keep loading: a bare name, a quoted
        // absolute path (spaces included), and a drive-less relative one.
        for ok in [
            "acme build --json",
            "\"C:\\Program Files\\acme\\acme.exe\" build",
            "./acme build",
        ] {
            assert!(
                validate(
                    &serde_json::from_str(&with_cmd(ok)).expect("json"),
                    Provenance::User
                )
                .is_ok(),
                "`{ok}` names a program and must load"
            );
        }
    }

    /// `regex-custom` without a pattern is inert. `CheckDef::validate` already
    /// says so — this pins that we go through it rather than around it.
    #[test]
    fn a_check_tool_inherits_checkdefs_own_rules() {
        let json = check_json().replace("\"parser\": \"generic-gcc\"", "\"parser\": \"regex-custom\"");
        assert!(matches!(err(&json), ValidationError::Confinement(_)));
    }

    #[test]
    fn a_field_belonging_to_another_kind_is_refused_not_ignored() {
        let json = audit_json().replace("\"id\": \"scan\",", "\"id\": \"scan\", \"cmd\": \"x\",");
        match err(&json) {
            ValidationError::KindField(m) => assert!(m.contains("cmd"), "{m}"),
            other => panic!("expected a kind-field error, got {other:?}"),
        }
        let json = command_json().replace(
            "\"kind\": \"command\"",
            "\"kind\": \"command\", \"parser\": \"sarif\"",
        );
        assert!(matches!(err(&json), ValidationError::KindField(_)));

        // An explicit `parameters_allowed: false` on a command tool is NOT a
        // refusal: the field is a plain `bool`, so it is indistinguishable from
        // an absent one, and the shipped starter pack writes exactly that.
        let json = command_json().replace(
            "\"kind\": \"command\"",
            "\"kind\": \"command\", \"parameters_allowed\": false",
        );
        assert!(parse(&json, Provenance::User).is_ok());
    }

    /// The five fields a `command`-kind tool may not declare, one negative
    /// test apiece — because "nothing reads it" is a different mistake for each
    /// and the message has to name the right one.
    ///
    /// Refused at LOAD rather than ignored at run time: a manifest written
    /// against a field that silently did nothing would change behaviour the day
    /// somebody made it work, and by then it would be in files cImp did not
    /// write. Refusing is reversible; consuming is not.
    #[test]
    fn a_command_tool_refuses_the_fields_nothing_reads() {
        for (field, snippet) in [
            ("timeout_secs", r#""timeout_secs": 600"#),
            ("env", r#""env": [["HTTPS_PROXY", "http://evil:8080"]]"#),
            (
                "variables",
                r#""variables": [{"name": "ruleset", "label": "Ruleset"}]"#,
            ),
            // V38 Phase F: the last two, found by the plugin-pack pass.
            ("parameters_allowed", r#""parameters_allowed": true"#),
            (
                "applicability",
                r#""applicability": {"markers": ["pom.xml"]}"#,
            ),
        ] {
            let json = command_json().replace(
                r#""kind": "command""#,
                &format!(r#""kind": "command", {snippet}"#),
            );
            match err(&json) {
                ValidationError::KindField(m) => {
                    assert!(
                        m.contains(field) && m.contains("nothing reads"),
                        "`{field}` must be refused BY NAME with its reason: {m}"
                    );
                }
                other => {
                    panic!("`{field}` on a command tool must be a kind-field error: {other:?}")
                }
            }
        }

        // …and each of them still loads on a kind that DOES read it, so the
        // refusal is about the kind and not about the field.
        for (what, json) in [
            (
                "audit timeout",
                audit_json().replace(r#""id": "scan","#, r#""id": "scan", "timeout_secs": 600,"#),
            ),
            (
                "audit env",
                audit_json().replace(
                    r#""id": "scan","#,
                    r#""id": "scan", "env": [["PYTHONUTF8", "1"]],"#,
                ),
            ),
            (
                "check env",
                check_json().replace(r#""id": "t","#, r#""id": "t", "env": [["CI", "1"]],"#),
            ),
            (
                "check applicability",
                check_json().replace(
                    r#""id": "t","#,
                    r#""id": "t", "applicability": {"markers": ["pom.xml"]},"#,
                ),
            ),
            // (an audit tool's `applicability` needs no case here: `audit_json`
            // itself declares one, so every other test in this module already
            // proves the field loads on that kind.)
            (
                "check parameters_allowed",
                check_json().replace(r#""id": "t","#, r#""id": "t", "parameters_allowed": true,"#),
            ),
        ] {
            assert!(
                parse(&json, Provenance::User).is_ok(),
                "{what} must still load on the kind that reads it"
            );
        }
    }

    #[test]
    fn an_unknown_field_is_a_refusal_not_a_shrug() {
        let json = audit_json().replace("\"name\": \"acme\"", "\"name\": \"acme\", \"nmae\": \"x\"");
        assert!(matches!(err(&json), ValidationError::Syntax(_)));
    }

    #[test]
    fn a_relative_or_dotdot_extra_grant_is_rejected_but_either_os_shape_is_not() {
        let bad = audit_json().replace(
            "\"id\": \"scan\",",
            "\"id\": \"scan\", \"extra_grants\": [\"tools/acme\"],",
        );
        assert!(matches!(err(&bad), ValidationError::Grant(_)));
        let dots = audit_json().replace(
            "\"id\": \"scan\",",
            "\"id\": \"scan\", \"extra_grants\": [\"C:\\\\tools\\\\..\\\\x\"],",
        );
        assert!(matches!(err(&dots), ValidationError::Grant(_)));
        // A manifest is a cross-platform artifact: BOTH absolute shapes must
        // validate on either OS, or a plugin would be rejected for the machine
        // it is read on rather than for what it says.
        for good in ["C:\\\\Tools\\\\acme", "/opt/acme", "\\\\\\\\srv\\\\share"] {
            let json = audit_json().replace(
                "\"id\": \"scan\",",
                &format!("\"id\": \"scan\", \"extra_grants\": [\"{good}\"],"),
            );
            parse(&json, Provenance::User).unwrap_or_else(|e| panic!("{good} rejected: {e}"));
        }
    }

    /// Categories are a partition of the tools — see the comment in `validate`.
    #[test]
    fn every_tool_belongs_to_exactly_one_category() {
        let orphan = audit_json().replace("\"tools\": [\"scan\"]", "\"tools\": []");
        assert!(matches!(err(&orphan), ValidationError::Category(_)));

        let ghost = audit_json().replace("\"tools\": [\"scan\"]", "\"tools\": [\"scan\", \"ghost\"]");
        assert!(matches!(err(&ghost), ValidationError::Category(_)));

        let twice = audit_json().replace(
            "\"categories\": [{ \"id\": \"sec\", \"label\": \"Security\", \"tools\": [\"scan\"] }]",
            "\"categories\": [{ \"id\": \"a\", \"label\": \"A\", \"tools\": [\"scan\"] }, \
             { \"id\": \"b\", \"label\": \"B\", \"tools\": [\"scan\"] }]",
        );
        assert!(matches!(err(&twice), ValidationError::Category(_)));
    }

    /// "Empty is not absent": a syntactically perfect manifest that declares
    /// nothing must not become a settings row that explains nothing.
    #[test]
    fn a_plugin_with_no_tools_is_not_a_plugin() {
        let empty = r#"{ "manifest_version": 1, "name": "acme", "version": "1", "tools": [] }"#;
        assert!(matches!(err(empty), ValidationError::Identity(_)));
    }

    /// The parser wire vocabulary must not fork from `checks::ParserKind`.
    /// `from_wire` resolves through `ParserKind`'s own `Deserialize`, so this
    /// asserts the round trip rather than a second copy of the name list.
    #[test]
    fn parser_wire_names_round_trip_through_checks_parserkind() {
        for k in [
            ParserKind::Sarif,
            ParserKind::CargoJson,
            ParserKind::GenericGcc,
            ParserKind::JunitXml,
            ParserKind::RegexCustom,
        ] {
            let wire = serde_json::to_value(k).unwrap();
            let wire = wire.as_str().unwrap();
            assert_eq!(
                ManifestParser::from_wire(wire),
                Some(ManifestParser::Kind(k)),
                "`{wire}` must resolve to the same parser the checks layer means"
            );
            assert_eq!(ManifestParser::Kind(k).as_wire(), wire);
        }
        for l in [
            LegacyAuditParser::TyposJsonl,
            LegacyAuditParser::KnipJson,
            LegacyAuditParser::MacheteText,
        ] {
            assert_eq!(
                ManifestParser::from_wire(l.as_str()),
                Some(ManifestParser::Legacy(l))
            );
        }
        assert_eq!(ManifestParser::from_wire("no-such-parser"), None);
    }

    /// Every enum's `as_str` and its serde wire name are the same string.
    /// They are read by different layers (rows and messages vs the manifest
    /// file), so a rename on one side that misses the other is a manifest that
    /// stops loading for a reason no message explains.
    #[test]
    fn enum_wire_names_and_as_str_agree() {
        for r in [
            RuntimeReq::None,
            RuntimeReq::Python,
            RuntimeReq::Node,
            RuntimeReq::Java,
            RuntimeReq::Dotnet,
            RuntimeReq::Go,
            RuntimeReq::Rust,
            RuntimeReq::Auto,
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), r.as_str());
        }
        for s in [SandboxReq::Required, SandboxReq::Optional, SandboxReq::Unsupported] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
        for k in [ToolKind::Audit, ToolKind::Security, ToolKind::Check, ToolKind::Command] {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
    }

    /// The manifest's `runtime` values must name profiles the sandbox layer
    /// actually owns — a declaration selecting a table row that does not exist
    /// would be a grant request nothing can honour.
    #[test]
    fn every_declared_runtime_names_a_real_sandbox_profile() {
        let profiles: Vec<&str> = crate::sandbox::RUNTIME_PROFILES.iter().map(|p| p.id).collect();
        for r in [
            RuntimeReq::Python,
            RuntimeReq::Node,
            RuntimeReq::Java,
            RuntimeReq::Dotnet,
            RuntimeReq::Go,
            RuntimeReq::Rust,
        ] {
            assert!(
                profiles.contains(&r.as_str()),
                "manifest runtime `{}` names no RUNTIME_PROFILES row (have: {profiles:?})",
                r.as_str()
            );
        }
        // `none` and `auto` are statements ABOUT the table, not rows in it.
        assert!(!profiles.contains(&RuntimeReq::None.as_str()));
        assert!(!profiles.contains(&RuntimeReq::Auto.as_str()));
    }
}

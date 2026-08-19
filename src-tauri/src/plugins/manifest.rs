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
    /// `manifest_version` is not [`MANIFEST_VERSION`].
    Version { found: u32 },
    /// `name`/`version`/`id`/`label` missing, empty, or outside the id charset.
    Identity(String),
    /// A user plugin claimed [`RESERVED_NAME_PREFIX`].
    ReservedName(String),
    /// A `builtin` field appeared in a manifest FILE. Provenance is stamped.
    BuiltinField(String),
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
    if !valid_version(&m.version) {
        return Err(ValidationError::Identity(format!(
            "`version` must be a non-empty [A-Za-z0-9._+-] token (found `{}`)",
            m.version
        )));
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
        if t.label.trim().is_empty() {
            return Err(ValidationError::Identity(format!(
                "tool `{}` has an empty `label`",
                t.id
            )));
        }
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
        if c.label.trim().is_empty() {
            return Err(ValidationError::Category(format!(
                "category `{}` has an empty `label`",
                c.id
            )));
        }
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

    let declared_vars: BTreeSet<&str> = t.variables.iter().map(|v| v.name.as_str()).collect();
    for v in &t.variables {
        if !valid_id(&v.name) {
            return Err(ValidationError::Identity(format!(
                "tool `{}` declares variable `{}`, which is not a [A-Za-z0-9_-] token",
                t.id, v.name
            )));
        }
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
            if t.argv.is_empty() {
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
            for arg in &t.argv {
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
            if !t.argv.is_empty() {
                return Err(wrong("argv", "audit/security"));
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

/// Parse + validate one manifest's bytes. The single entry point the loader
/// uses, so "parsed" and "validated" cannot come apart.
pub fn parse(text: &str, provenance: Provenance) -> Result<PluginManifest, ValidationError> {
    let m: PluginManifest =
        serde_json::from_str(text).map_err(|e| ValidationError::Syntax(e.to_string()))?;
    validate(&m, provenance)?;
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
        parse(json, Provenance::User).expect_err("manifest should have been rejected")
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
            Err(ValidationError::BuiltinField(_))
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

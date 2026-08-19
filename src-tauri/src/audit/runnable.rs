//! V38 Phase C — the **owned runnable form** a plugin tool takes on its way
//! into the audit fan-out, and the identity both populations are keyed by.
//!
//! The built-in tier is a table of `static Adapter`s selected by a closed
//! `AuditToolId` enum; a plugin tool is a manifest joined with user state at
//! invocation time, so nothing about it can be `static` and its id is a string
//! this build has never seen. Those two facts are the whole reason this module
//! exists — everything else about a run (spawn, timeout, cancel, output caps,
//! the sandbox decision, `ToolState`, the status event, the report) is
//! deliberately SHARED, because a plugin tool that took a second path through
//! the runner would be a second place for every rule to be forgotten.
//!
//! What lives here:
//! * [`ToolKey`] — the id a `ToolState` and an `AuditFinding` carry now: a
//!   built-in's kebab wire id, or a plugin's `name@version/tool-id`. Both
//!   serialize as a plain string, so the wire is unchanged for the 14 built-ins.
//! * [`RunnableAudit`] — one plugin tool, resolved: everything `run_one` needs,
//!   owned, plus the manifest's sandbox posture.
//! * [`render_argv`] — the token substitution, **single-pass** (see its docs;
//!   this is a security property, not a performance one).
//! * [`sarif_envelope`] — the ingest gate that decides whether output that
//!   parsed is output that *said* anything (decision 3's substantiveness rule).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Serialize, Serializer};

use super::adapters::{Category, Transport};
use super::census::Census;
use crate::plugins::manifest::{
    self, IngestReq, LegacyAuditParser, ManifestParser, Provenance, ProviderRef, RuntimeReq,
    SandboxReq, ToolKind,
};
use crate::plugins::registry::EffectiveTool;

/// Which tool a `ToolState` / `AuditFinding` belongs to.
///
/// A plugin's key always contains `@` and `/`; a built-in's kebab id never
/// does — so the two namespaces cannot collide, and a plugin can never present
/// itself to the UI, the report or the filter bar AS a built-in. That is not a
/// coincidence to rely on quietly: it is asserted by
/// `a_plugin_key_can_never_collide_with_a_builtin_wire_id`.
///
/// # Why a built-in is a bare string since V38 Phase E
///
/// It used to be `AuditToolId`, a closed enum, and the enum is gone: the
/// fourteen tools are embedded manifests now, so their ids are strings this
/// build reads from JSON like any other. The **wire spelling is unchanged** —
/// `osv-scanner`, `semgrep-quality` — because the Code Audit view, the report a
/// model reads, the findings filter and every settings file key off it. What a
/// built-in is called did not move; only where the name is written down.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolKey {
    /// A tool from one of cImp's embedded manifests, by its manifest-local id
    /// (`osv-scanner`). Never namespaced: a built-in's id IS its public name.
    Builtin(String),
    /// A user plugin's tool, by `plugins::loader::LoadedPlugin::tool_key`
    /// (`name@version/tool-id`).
    Plugin(String),
}

impl ToolKey {
    /// The wire string every consumer keys off.
    pub fn wire(&self) -> String {
        match self {
            ToolKey::Builtin(id) => id.clone(),
            ToolKey::Plugin(key) => key.clone(),
        }
    }

    /// The identity of one resolved tool: its provenance decides which of the
    /// two namespaces it lands in, and nothing else does. Keyed off the
    /// loader's stamp rather than off a name, which is the rule R3 exists for.
    pub fn of(tool: &EffectiveTool) -> Self {
        match tool.provenance {
            Provenance::Builtin => ToolKey::Builtin(tool.tool_id.clone()),
            Provenance::User => ToolKey::Plugin(tool.tool_key.clone()),
        }
    }

    /// Whether this is a built-in with the given id — the one question the
    /// osv-scanner coverage pass and the security-floor tests ask. Spelled as a
    /// method so no caller matches on the variant and starts branching on
    /// built-in-ness for reasons of its own.
    pub fn is_builtin(&self, id: &str) -> bool {
        matches!(self, ToolKey::Builtin(b) if b == id)
    }
}

impl Serialize for ToolKey {
    /// Both variants are their wire string. A built-in serialized exactly as
    /// `AuditToolId` did before V38, which is what keeps every stored report,
    /// settings file and TS mirror valid across the migration.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.wire())
    }
}

/// One plugin tool, fully resolved and ready to run — the owned twin of a
/// `static Adapter` + its `AuditToolConfig`.
///
/// Owned by construction: the manifest it came from lives in a `PluginSet`
/// snapshot the runner has no reason to hold across an await, and the user
/// state it was joined with is a settings snapshot that may be replaced
/// mid-scan. A scan that started under one configuration runs to completion
/// under it — the same rule `begin_scan` already applies to the sandbox config.
#[derive(Clone, Debug)]
pub struct RunnableAudit {
    pub key: ToolKey,
    /// The manifest's `label` — user-facing text for error messages.
    pub label: String,
    /// Which umbrella this tool fans out under.
    pub category: Category,
    /// The configured binary path, verbatim, or empty when there is none.
    ///
    /// Empty is only legal for a built-in (see [`command`](Self::command)):
    /// decision 7 — cImp never picks a binary for a definition it did not ship.
    pub program: String,
    /// The bare command name to resolve through `ebin` → `PATH` when
    /// [`program`](Self::program) is empty. `None` for a user plugin, always
    /// `Some` for a built-in — the narrow, provenance-gated relaxation that
    /// keeps the fourteen shipped scanners working without a configured path.
    pub command: Option<String>,
    /// A `node_modules/.bin` shim name preferred over a global install when no
    /// path is configured (eslint, knip).
    pub project_local_bin: Option<String>,
    /// The manifest's argv template, tokens unsubstituted.
    pub argv: Vec<String>,
    /// The template used when the scan root is not a git repository. Empty =
    /// [`argv`](Self::argv) is used either way.
    pub dir_argv: Vec<String>,
    /// Declared variables, layered (manifest default → user value) by the
    /// registry. **Untrusted**: values ride the project overlay.
    pub variables: BTreeMap<String, String>,
    /// The user's appended argv, verbatim (the `extra_args` successor).
    pub parameters: Vec<String>,
    pub transport: Transport,
    pub env: Vec<(String, String)>,
    pub findings_exit_codes: Vec<i32>,
    pub timeout_secs: Option<u64>,
    /// The FINDINGS parser (G2's namespace rule), resolved at plan time.
    pub parser: AuditParser,
    /// The gate this tool's output passes before any of it becomes a finding —
    /// resolved once, here, from the manifest's declaration and the resolved
    /// parser. See [`IngestGate`].
    pub gate: IngestGate,
    pub runtime: RuntimeReq,
    pub sandbox: SandboxReq,
    pub extra_grants: Vec<String>,
    /// Loader-stamped, never claimed by a file. Carried so the invariants that
    /// must key off provenance (R3: the security floor, the parser rule) can do
    /// so without matching on a NAME — and so Phase E's embedded built-ins
    /// arrive here already distinguishable. Read by those tests today.
    #[allow(dead_code)]
    pub provenance: Provenance,
    /// The manifest's applicability gate, kept so planning can apply the same
    /// census test the built-in adapters get.
    pub applicability: manifest::Applicability,
    /// V38 Phase F — set for a **tier-2** tool: the MCP server and tool name its
    /// findings come from. `Some` means nothing is spawned for this tool, so the
    /// runner takes the provider path and every spawn-shaped field above
    /// (`program`, `argv`, `transport`, the posture trio) is at its inert
    /// default by validation.
    pub provider: Option<ProviderRef>,
}

impl RunnableAudit {
    /// Join one [`EffectiveTool`] into a runnable audit tool, or say why not.
    ///
    /// `Ok(None)` = "not an audit-umbrella tool at all" (a `check`/`command`
    /// kind, Phase D's population) — not an error, just not ours. `Err` is a
    /// tool that BELONGS here and cannot run, which the caller surfaces as a
    /// failed chip rather than dropping: a tool the user enabled and pointed at
    /// a binary must never disappear from the report in silence.
    pub fn from_effective(tool: &EffectiveTool) -> Result<Option<Self>, String> {
        let category = match tool.kind() {
            ToolKind::Security => Category::Security,
            ToolKind::Audit => Category::Quality,
            ToolKind::Check | ToolKind::Command => return Ok(None),
        };
        // A tool with neither a configured path nor a name cImp may resolve for
        // it cannot be spawned. `runnable_tools` filters these out, so reaching
        // here is a caller bug rather than a tool error — but it is reported as
        // a reason rather than dropped, because a tool the user enabled must not
        // vanish from a report in silence either way.
        let program = tool.path.clone().unwrap_or_default();
        // A tier-2 tool has no binary by construction — the whole point is that
        // a server the user administers answers instead. Everything below is
        // shared with tier 1 on purpose: the key, the category, the timeout, the
        // parser and the ingest gate are facts about the TOOL, not about who
        // produced its bytes.
        if !tool.is_provider() && program.is_empty() && !tool.resolves_by_name() {
            return Err("no binary path is configured".to_string());
        }
        let parser = findings_parser(tool.manifest.parser)?;
        Ok(Some(Self {
            key: ToolKey::of(tool),
            label: tool.manifest.label.clone(),
            category,
            program,
            command: tool
                .resolves_by_name()
                .then(|| tool.manifest.command.clone())
                .flatten(),
            project_local_bin: tool.manifest.project_local_bin.clone(),
            argv: tool.manifest.argv.clone(),
            dir_argv: tool.manifest.dir_argv.clone(),
            variables: tool.variables.clone(),
            parameters: tool.parameters.clone(),
            transport: match tool.manifest.transport {
                Some(manifest::Transport::ReportFile) => Transport::ReportFile,
                _ => Transport::Stdout,
            },
            env: tool.manifest.env.clone(),
            findings_exit_codes: tool.manifest.findings_exit_codes.clone(),
            timeout_secs: tool.timeout_secs,
            parser,
            // The manifest's grandfathering, resolved here rather than at the
            // spawn site: which gate applies is a fact about the TOOL, and a
            // runner that re-derived it would be a second place for "built-in
            // semantics" to be spelled differently.
            gate: match tool.manifest.ingest {
                Some(IngestReq::Grandfathered) => IngestGate::None,
                None => IngestGate::for_parser(parser),
            },
            runtime: tool.manifest.runtime,
            sandbox: tool.manifest.sandbox,
            extra_grants: tool.manifest.extra_grants.clone(),
            provenance: tool.provenance,
            applicability: tool.manifest.applicability.clone(),
            provider: tool.manifest.provider.clone(),
        }))
    }

    /// Whether this tool's manifest gate admits the project.
    ///
    /// Delegates to [`Census::admits`], which since V38 Phase F is the ONE
    /// statement of the rule: `checks::plugin` gates its `check`-kind
    /// population with the same function, and a tool that was applicable under
    /// an umbrella but not under `run_check` (or the reverse) would be one
    /// manifest field meaning two things.
    pub fn applicable(&self, census: &Census) -> bool {
        census.admits(&self.applicability)
    }

    /// The full argv: the substituted template, then the user's parameters
    /// appended verbatim (the `extra_args` contract).
    ///
    /// `git_repo` selects [`dir_argv`](Self::dir_argv) when the scan root is not
    /// a git repository and the tool declared one (gitleaks' `dir` form). A tool
    /// that declared none is indifferent and gets [`argv`](Self::argv) either
    /// way — the same rule the built-in adapter table applied before V38.
    pub fn full_argv(&self, root: &Path, report: Option<&Path>, git_repo: bool) -> Vec<String> {
        let template = if !git_repo && !self.dir_argv.is_empty() {
            &self.dir_argv
        } else {
            &self.argv
        };
        let mut argv = render_argv(template, &self.variables, root, report);
        argv.extend(self.parameters.iter().cloned());
        argv
    }

    /// What the sandbox lane, the spawn ledger and the activity row call this
    /// run.
    ///
    /// A built-in answers with its COMMAND name (`semgrep`), which is what those
    /// rows have said since V33 — they are about the program that ran, and a
    /// user grepping `audit:semgrep` in the sandbox lane after an upgrade must
    /// still find it. A plugin has no such name and answers with its key, which
    /// is the only identity it has.
    pub fn spawn_subject(&self) -> String {
        self.command.clone().unwrap_or_else(|| self.key.wire())
    }

    /// Which sandbox runtime profile this tool's grants come from.
    ///
    /// Delegated to `plugins::posture` since Phase D: three seams translate this
    /// vocabulary now, and one of them disagreeing about what `auto` means would
    /// be a boundary difference nobody could see.
    pub fn runtime_select(&self) -> crate::sandbox::RuntimeSelect {
        crate::plugins::posture::runtime_select(self.runtime)
    }
}

/// Which decoder turns an audit tool's captured output into [`Diag`]s — the
/// **findings** namespace of G2's kind rule.
///
/// # Why this is not `checks::ParserKind`
///
/// It is the same set of decoders: every arm below delegates to
/// [`crate::checks::parsers::parse`], because decision 3 wants ONE parse
/// boundary and two dispatch tables over the same decoders would be two places
/// to register the next one — with only one of them getting it.
///
/// What this type carries is the NAMESPACE. A manifest's `parser` word means a
/// different thing on an `audit`/`security` tool than on a `check`
/// (`eslint-json` decodes findings here and diagnostics there), and the
/// difference is not cosmetic: decoding output with the wrong decoder yields
/// zero results, which reads exactly like a clean scan. So the resolution is
/// kind-aware ([`findings_parser`]) and lands in a type that can only be a
/// findings decoder.
///
/// V38 Phase E moved it here from `audit/parsers.rs`, which by then held only
/// this enum and had no reason to be a module: it belongs beside the function
/// that resolves into it and the runner that dispatches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditParser {
    /// SARIF 2.1 — the contract for every user plugin, and for most built-ins.
    Sarif,
    /// ESLint `--format json`, then relativized (see [`AuditParser::parse`]).
    EslintJson,
    /// `typos --format json` — one JSON object per line.
    TyposJsonl,
    /// knip `--reporter json` — one document, `{ issues: [...] }`.
    KnipJson,
    /// `cargo-machete` text output.
    MacheteText,
}

impl AuditParser {
    /// Decode `output` (a tool's stdout or report-file contents) into findings,
    /// with every path made project-relative to `root`. Total and lenient —
    /// malformed input yields an empty list, never an error, because "what did
    /// this tool say" is answered by the ingest gate before this runs.
    pub fn parse(self, output: &str, root: &Path) -> Vec<crate::checks::Diag> {
        use crate::checks::ParserKind;
        let kind = match self {
            AuditParser::Sarif => ParserKind::Sarif,
            AuditParser::EslintJson => ParserKind::EslintJson,
            AuditParser::TyposJsonl => ParserKind::TyposJsonl,
            AuditParser::KnipJson => ParserKind::KnipJson,
            AuditParser::MacheteText => ParserKind::MacheteText,
        };
        let mut diags = crate::checks::parsers::parse(kind, output, "", root, None);
        // The ONE behaviour that is this layer's own rather than the checks
        // layer's: an audit report presents project-relative paths, and the
        // shared eslint decoder leaves `filePath` absolute because `checks::run`
        // relativizes downstream — a step the audit runner does not have.
        if self == AuditParser::EslintJson {
            for d in &mut diags {
                if !d.file.is_empty() {
                    d.file = crate::checks::parsers::relativize(root, &d.file);
                }
            }
        }
        diags
    }
}

/// Resolve a manifest's `parser` value in the **findings** namespace (G2).
///
/// The wire string is disambiguated by KIND, so this is where an audit-kind
/// tool's `parser` becomes an [`AuditParser`] and nothing else. A user plugin
/// can only ever reach the `sarif` arm — `manifest::validate` rejects anything
/// else on an audit/security tool — so the remaining arms exist for Phase E's
/// embedded built-ins, and a value from the *diagnostics* namespace is an
/// error rather than a silently substituted default: decoding a tool's output
/// with the wrong parser produces zero findings, which reads exactly like a
/// clean scan.
fn findings_parser(parser: Option<ManifestParser>) -> Result<AuditParser, String> {
    use crate::checks::ParserKind;
    Ok(match parser {
        None | Some(ManifestParser::Kind(ParserKind::Sarif)) => AuditParser::Sarif,
        Some(ManifestParser::Kind(ParserKind::EslintJson)) => AuditParser::EslintJson,
        Some(ManifestParser::Legacy(LegacyAuditParser::TyposJsonl)) => AuditParser::TyposJsonl,
        Some(ManifestParser::Legacy(LegacyAuditParser::KnipJson)) => AuditParser::KnipJson,
        Some(ManifestParser::Legacy(LegacyAuditParser::MacheteText)) => AuditParser::MacheteText,
        Some(other) => {
            return Err(format!(
                "its manifest names the `{}` parser, which decodes DIAGNOSTICS for `run_check` \
                 and not findings — an audit or security tool must deliver SARIF",
                other.as_wire()
            ))
        }
    })
}

/// Substitute `{root}`, `{report}` and `{var:NAME}` into an argv template —
/// **once**, left to right, never over the result.
///
/// # Single-pass is the security property
///
/// A variable's value comes from the project overlay (`.cimp/config.json`),
/// which lives inside the project root — a directory every sandboxed child is
/// granted full access to. So a variable value is attacker-reachable input, and
/// a substitution that re-scanned its own output would let
/// `ruleset = "{report}"` name cImp's report path, or `{var:a}` expand into
/// `{var:b}`. Values are copied into the output and never looked at again.
///
/// `{{` is a literal `{` (an escape has to exist, or a tool needing a real
/// brace has no expression). An unknown or unterminated token is copied through
/// verbatim: validation already refused those at load, and inventing an empty
/// string here would turn a manifest bug into a subtly wrong command line.
/// A `{var:NAME}` naming an undeclared variable renders empty — the registry
/// layers declared names only, so "declared but unset" and "never declared"
/// reach this the same way, and an empty argument is the honest rendering of a
/// value that does not exist.
pub fn render_argv(
    template: &[String],
    variables: &BTreeMap<String, String>,
    root: &Path,
    report: Option<&Path>,
) -> Vec<String> {
    let root_s = root.to_string_lossy().into_owned();
    let report_s = report
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    template
        .iter()
        .map(|arg| substitute(arg, &root_s, &report_s, variables))
        .collect()
}

fn substitute(
    arg: &str,
    root: &str,
    report: &str,
    variables: &BTreeMap<String, String>,
) -> String {
    let mut out = String::with_capacity(arg.len());
    let bytes = arg.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            // Not a token start: copy this byte's whole character.
            let ch_len = arg[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&arg[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'{') {
            out.push('{');
            i += 2;
            continue;
        }
        let Some(rel_end) = arg[i + 1..].find('}') else {
            // Unterminated — the rest is literal.
            out.push_str(&arg[i..]);
            break;
        };
        let inner = &arg[i + 1..i + 1 + rel_end];
        match inner {
            "root" => out.push_str(root),
            "report" => out.push_str(report),
            _ => match inner.strip_prefix("var:") {
                // The substituted value is pushed and `i` moves PAST the token:
                // nothing that was just written is ever re-examined.
                Some(name) => out.push_str(variables.get(name).map(String::as_str).unwrap_or("")),
                None => out.push_str(&arg[i..i + 1 + rel_end + 1]),
            },
        }
        i += 1 + rel_end + 1;
    }
    out
}

/// Which gate a tool's output passes before its findings enter a report.
///
/// **Keyed on the RESOLVED parser, never on the manifest's wire string** (G2):
/// the same word means different decoders in the findings and diagnostics
/// namespaces, so the decision has to be made after resolution or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestGate {
    /// The built-in adapter tier. Its semantics were measured against fourteen
    /// real tools (gitleaks writes no report when clean; cppcheck always exits
    /// 0) and V38 does not retro-fit a contract onto them — R4.
    None,
    /// The full SARIF envelope: parses, and is a SARIF LOG. Every user plugin's
    /// audit/security tool, whose `parser` validation forces `sarif`.
    Sarif,
    /// Substantiveness only, for a findings parser that is NOT SARIF — the
    /// legacy JSONL/JSON/text decoders, which Phase E's embedded built-ins are
    /// the only thing that can select. There is no envelope to check (a
    /// `typos` JSONL stream has no schema to recognize), but "the tool produced
    /// nothing" is still not the same claim as "the tool found nothing".
    Substantive,
}

impl IngestGate {
    /// The gate for a resolved findings parser on a PLUGIN tool.
    pub fn for_parser(parser: AuditParser) -> Self {
        match parser {
            AuditParser::Sarif => IngestGate::Sarif,
            _ => IngestGate::Substantive,
        }
    }

    /// Apply it. `Ok(())` = these findings may be read.
    pub fn check(self, text: &str) -> Result<(), String> {
        match self {
            IngestGate::None => Ok(()),
            IngestGate::Sarif => sarif_envelope(text),
            IngestGate::Substantive => substantive_output(text),
        }
    }
}

/// The weakest honest gate: did the tool write ANYTHING?
///
/// Used where there is no envelope to recognize. It is deliberately not
/// "and it decoded into findings" — a tool that ran and found nothing writes
/// real output that decodes to zero findings, and that is a clean pass.
fn substantive_output(text: &str) -> Result<(), String> {
    if text.trim_start_matches('\u{feff}').trim().is_empty() {
        return Err(
            "the tool produced no output at all — nothing was decoded, so this run is not \
             evidence of a clean project, only of a tool that said nothing"
                .to_string(),
        );
    }
    Ok(())
}

/// Whether a plugin tool's output is a SARIF log that **says something** —
/// decision 3's envelope validation, run before any finding enters a report.
///
/// Three questions, in the order they can be answered:
///
/// 1. **Is there anything at all?** Empty or whitespace-only output is a tool
///    that delivered nothing. Not "clean" — *nothing*.
/// 2. **Is it JSON?** A tool that printed a usage message on stdout, or wrote a
///    half-flushed file, parses as neither.
/// 3. **Is it a SARIF LOG?** `{"runs": [...]}` is the envelope; an arbitrary
///    JSON document that happens to parse (`{}`, `[]`, `"ok"`, a config file the
///    tool echoed) is not, and reading zero findings out of it would report a
///    clean scan from a tool whose output cImp never understood.
///
/// **`runs: []` is substantive and clean.** That is the whole point of the
/// distinction: a SARIF log with an empty results array is a tool saying "I ran
/// and found nothing", which is exactly what a clean scan looks like, while an
/// empty file is a tool saying nothing at all. Empty is not absent.
///
/// The tool's own identity claim (`runs[].tool.driver.name`) is deliberately
/// NOT checked: attribution comes from the registry entry that was spawned, and
/// a name inside output is a claim by the thing being audited.
pub fn sarif_envelope(text: &str) -> Result<(), String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Err(
            "the tool produced no output at all — an audit tool's contract is a SARIF log, and an \
             empty artifact is not a clean scan, it is a tool that said nothing (a SARIF log with \
             `\"runs\": []` is how a clean run reports zero findings)"
                .to_string(),
        );
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Err(
            "the tool's output is not JSON, so it cannot be the SARIF log this tool's contract \
             promises — findings were not read"
                .to_string(),
        );
    };
    if !value.get("runs").is_some_and(|r| r.is_array()) {
        return Err(
            "the tool's output parses as JSON but is not a SARIF log (no `runs` array) — cImp \
             will not read zero findings out of a document it does not understand and call that \
             a clean scan"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Severity;
    use std::path::PathBuf;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn root() -> PathBuf {
        PathBuf::from("C:\\proj")
    }

    /// The binding constraint: a variable value is data, never a template. It
    /// rides the project overlay, which lives inside the one directory every
    /// sandboxed child can write — so a value that LOOKS like a token has to
    /// land in argv as those literal characters.
    #[test]
    fn a_variable_value_is_never_rescanned_for_tokens() {
        let argv = render_argv(
            &["--rules".to_string(), "{var:ruleset}".to_string()],
            &vars(&[("ruleset", "{report}")]),
            &root(),
            Some(Path::new("C:\\tmp\\r.sarif")),
        );
        assert_eq!(argv, vec!["--rules", "{report}"], "substituted, not expanded");

        // …and the same for the other two tokens, including a value that
        // names a variable of its own.
        let argv = render_argv(
            &["{var:a}".to_string(), "{var:b}".to_string()],
            &vars(&[("a", "{root}"), ("b", "{var:a}")]),
            &root(),
            None,
        );
        assert_eq!(argv, vec!["{root}", "{var:a}"]);
    }

    #[test]
    fn tokens_substitute_and_braces_escape() {
        let argv = render_argv(
            &[
                "--out={report}".to_string(),
                "{root}".to_string(),
                // `{{` is the escape; a bare `}` was never special.
                "{{literal}".to_string(),
                "prefix-{var:x}-suffix".to_string(),
                "{var:missing}".to_string(),
            ],
            &vars(&[("x", "VAL")]),
            &root(),
            Some(Path::new("C:\\tmp\\r.sarif")),
        );
        assert_eq!(
            argv,
            vec![
                "--out=C:\\tmp\\r.sarif",
                "C:\\proj",
                "{literal}",
                "prefix-VAL-suffix",
                "",
            ]
        );
    }

    /// A stdout-transport tool gets an empty `{report}` (there is no report
    /// path), and a non-ASCII argument survives byte-wise walking intact.
    #[test]
    fn report_is_empty_without_a_path_and_unicode_survives() {
        let argv = render_argv(
            &["{report}".to_string(), "café–{var:x}".to_string()],
            &vars(&[("x", "ü")]),
            &root(),
            None,
        );
        assert_eq!(argv, vec!["", "café–ü"]);
    }

    /// The substantiveness matrix, in one place: what counts as a clean scan
    /// and what is a tool error. `runs: []` is the ONLY empty-looking thing
    /// that is allowed to read as clean.
    #[test]
    fn the_envelope_separates_a_clean_run_from_a_blank_artifact() {
        assert!(sarif_envelope(r#"{"version":"2.1.0","runs":[]}"#).is_ok());
        assert!(sarif_envelope(r#"{"runs":[{"results":[]}]}"#).is_ok());
        // A BOM-prefixed log is still a log (Windows tools write them).
        assert!(sarif_envelope("\u{feff}{\"runs\":[]}").is_ok());

        for blank in ["", "   \n\t ", "\u{feff}"] {
            let e = sarif_envelope(blank).expect_err("blank output is not a clean scan");
            assert!(e.contains("no output at all"), "{e}");
        }
        for not_json in ["usage: acme [options]", "{\"runs\": [", "<xml/>"] {
            let e = sarif_envelope(not_json).expect_err("non-JSON is not a clean scan");
            assert!(e.contains("not JSON"), "{e}");
        }
        for parseable in ["{}", "[]", "\"ok\"", "null", r#"{"runs": {}}"#] {
            let e = sarif_envelope(parseable).expect_err("parseable is not the same as SARIF");
            assert!(e.contains("not a SARIF log"), "{e}");
        }
    }

    /// G2: the gate is chosen by the RESOLVED parser. A non-SARIF findings
    /// decoder (Phase E's embedded built-ins are the only thing that can select
    /// one) has no envelope to recognize, but "produced nothing" is still not
    /// "found nothing".
    #[test]
    fn the_ingest_gate_follows_the_resolved_parser() {
        assert_eq!(IngestGate::for_parser(AuditParser::Sarif), IngestGate::Sarif);
        for p in [
            AuditParser::TyposJsonl,
            AuditParser::KnipJson,
            AuditParser::MacheteText,
            AuditParser::EslintJson,
        ] {
            assert_eq!(IngestGate::for_parser(p), IngestGate::Substantive);
        }

        // Substantive: any real output passes, blank does not — and a typos
        // JSONL stream is not JSON as a whole, so the SARIF gate would have
        // been the wrong question entirely.
        assert!(IngestGate::Substantive
            .check(r#"{"type":"typo","path":"a.rs"}"#)
            .is_ok());
        assert!(IngestGate::Substantive.check("no unused dependencies").is_ok());
        let e = IngestGate::Substantive.check("  
 ").expect_err("blank");
        assert!(e.contains("no output at all"), "{e}");

        // …and the built-in tier's gate answers yes to everything, including
        // the empty report a clean gitleaks run leaves behind.
        assert!(IngestGate::None.check("").is_ok());
    }

    // ── the findings decoders (moved here with `AuditParser` in Phase E) ──
    //
    // Every arm delegates to `checks::parsers`, so these do not re-test the
    // decoders — they pin the DELEGATION and the one behaviour that is this
    // layer's own (the eslint relativization). A wrong arm would decode a
    // tool's output with another tool's parser and report zero findings,
    // which is indistinguishable from a clean scan.

    /// The scan root the decoder fixtures below are written against — POSIX,
    /// because their inputs carry POSIX `file://` uris and absolute paths, and
    /// `relativize` is string-based so the answer is the same on both platforms.
    fn decoder_root() -> PathBuf {
        PathBuf::from("/proj/root")
    }

    // ── SARIF delegation ────────────────────────────────────────────────────

    /// The `Sarif` arm delegates to the shared parser (a ruff-style SARIF) and
    /// relativizes its `file://` uri against the root.
    #[test]
    fn sarif_delegates_and_relativizes() {
        const SARIF: &str = r#"{
          "version": "2.1.0",
          "runs": [{
            "tool": { "driver": { "name": "ruff" } },
            "results": [{
              "ruleId": "F401",
              "level": "warning",
              "message": { "text": "`os` imported but unused" },
              "locations": [{
                "physicalLocation": {
                  "artifactLocation": { "uri": "file:///proj/root/app/main.py" },
                  "region": { "startLine": 1, "startColumn": 8 }
                }
              }]
            }]
          }]
        }"#;
        let d = AuditParser::Sarif.parse(SARIF, &decoder_root());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code.as_deref(), Some("F401"));
        assert_eq!(d[0].severity, Severity::Warning);
        assert_eq!(d[0].file, "app/main.py");
        assert_eq!(d[0].line, 1);
        assert_eq!(d[0].col, Some(8));
    }

    // ── EslintJson ──────────────────────────────────────────────────────────

    /// ESLint `--format json`: severity 2→error, 1→warning; ruleId→code; the
    /// absolute `filePath` relativizes against the root.
    #[test]
    fn eslint_json_severity_clamp_and_relativize() {
        let json = r#"[
          {
            "filePath": "/proj/root/src/app.ts",
            "messages": [
              { "ruleId": "no-unused-vars", "severity": 2, "message": "x is unused", "line": 3, "column": 7 },
              { "ruleId": "eqeqeq", "severity": 1, "message": "use ===", "line": 10, "column": 5 }
            ]
          }
        ]"#;
        let d = AuditParser::EslintJson.parse(json, &decoder_root());
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].severity, Severity::Error);
        assert_eq!(d[0].code.as_deref(), Some("no-unused-vars"));
        assert_eq!(d[0].file, "src/app.ts", "absolute filePath relativized");
        assert_eq!(d[0].line, 3);
        assert_eq!(d[0].col, Some(7));
        assert_eq!(d[1].severity, Severity::Warning);
    }

    // ── TyposJsonl ──────────────────────────────────────────────────────────

    /// Real typos JSONL: a `typo` with corrections → a note "`word` should be
    /// `correction`"; a non-typo line (e.g. a `binary_file` message) is ignored;
    /// a typo with `null` corrections degrades to "is misspelled".
    #[test]
    fn typos_jsonl_maps_typo_records() {
        let jsonl = concat!(
            r#"{"type":"binary_file","path":"assets/logo.png"}"#,
            "\n",
            r#"{"type":"typo","path":"src/main.rs","line_num":42,"byte_offset":11,"typo":"funciton","corrections":["function"]}"#,
            "\n",
            r#"{"type":"typo","path":"README.md","line_num":3,"byte_offset":0,"typo":"asdfg","corrections":null}"#,
            "\n",
            "not json, skipped",
        );
        let d = AuditParser::TyposJsonl.parse(jsonl, &decoder_root());
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].severity, Severity::Note);
        assert_eq!(d[0].code.as_deref(), Some("typo"));
        assert_eq!(d[0].message, "`funciton` should be `function`");
        assert_eq!(d[0].file, "src/main.rs");
        assert_eq!(d[0].line, 42);
        assert_eq!(d[0].col, Some(12)); // byte_offset 11 → 1-based 12
                                        // A typo with no correction still surfaces.
        assert_eq!(d[1].message, "`asdfg` is misspelled");
        assert_eq!(d[1].file, "README.md");
    }

    /// Multiple corrections are joined; an absolute typos path relativizes.
    #[test]
    fn typos_jsonl_multiple_corrections_and_abs_path() {
        let jsonl = r#"{"type":"typo","path":"/proj/root/docs/x.md","line_num":1,"byte_offset":4,"typo":"teh","corrections":["the","tech"]}"#;
        let d = AuditParser::TyposJsonl.parse(jsonl, &decoder_root());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "`teh` should be `the` or `tech`");
        assert_eq!(d[0].file, "docs/x.md");
    }

    // ── KnipJson ────────────────────────────────────────────────────────────

    /// knip JSON: an unused file, an unused export (with position), and an
    /// unused dependency in package.json — all `warning`.
    #[test]
    fn knip_json_files_exports_deps() {
        let json = r#"{
          "issues": [
            { "file": "src/legacy.ts", "files": [{ "name": "src/legacy.ts" }] },
            { "file": "src/math.ts", "exports": [{ "name": "factorial", "line": 12, "col": 14 }], "types": [{ "name": "Radians", "line": 20 }] },
            { "file": "package.json", "dependencies": [{ "name": "lodash" }], "unlisted": [{ "name": "rimraf" }] }
          ]
        }"#;
        let d = AuditParser::KnipJson.parse(json, &decoder_root());
        assert_eq!(d.len(), 5);
        assert!(d.iter().all(|x| x.severity == Severity::Warning));
        let unused_file = &d[0];
        assert_eq!(unused_file.code.as_deref(), Some("unused-file"));
        assert_eq!(unused_file.file, "src/legacy.ts");
        let export = d
            .iter()
            .find(|x| x.code.as_deref() == Some("unused-export"))
            .unwrap();
        assert_eq!(export.message, "unused export `factorial`");
        assert_eq!(export.file, "src/math.ts");
        assert_eq!(export.line, 12);
        assert_eq!(export.col, Some(14));
        let ty = d
            .iter()
            .find(|x| x.code.as_deref() == Some("unused-type"))
            .unwrap();
        assert_eq!(ty.message, "unused type `Radians`");
        let dep = d
            .iter()
            .find(|x| x.code.as_deref() == Some("unused-dependency"))
            .unwrap();
        assert_eq!(dep.message, "unused dependency `lodash`");
        assert_eq!(dep.file, "package.json");
        let unlisted = d
            .iter()
            .find(|x| x.code.as_deref() == Some("unlisted-dependency"))
            .unwrap();
        assert_eq!(unlisted.message, "unlisted dependency `rimraf`");
    }

    /// Malformed knip JSON yields no findings (module posture), never an error.
    #[test]
    fn knip_json_malformed_is_empty() {
        assert!(AuditParser::KnipJson.parse("not json", &decoder_root()).is_empty());
        assert!(AuditParser::KnipJson
            .parse(r#"{"issues":[]}"#, &decoder_root())
            .is_empty());
    }

    // ── MacheteText ─────────────────────────────────────────────────────────

    /// cargo-machete's real header + tab-indented crate list → one warning per
    /// crate, anchored to the reported Cargo.toml.
    #[test]
    fn machete_text_parses_header_and_indented_crates() {
        let text = concat!(
            "cargo-machete found the following unused dependencies in /proj/root/crate-a:\n",
            "\tserde\n",
            "\tregex\n",
        );
        let d = AuditParser::MacheteText.parse(text, &decoder_root());
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|x| x.severity == Severity::Warning));
        assert_eq!(d[0].code.as_deref(), Some("unused-dependency"));
        assert_eq!(d[0].message, "`serde` — unused dependency");
        assert_eq!(d[0].file, "crate-a/Cargo.toml");
        assert_eq!(d[1].message, "`regex` — unused dependency");
    }

    /// A location that already points at a Cargo.toml is used verbatim; crates
    /// under separate headers anchor to their own manifest.
    #[test]
    fn machete_text_manifest_location_and_multiple_headers() {
        let text = concat!(
            "cargo-machete found the following unused dependencies in /proj/root/Cargo.toml:\n",
            "\tlazy_static\n",
            "cargo-machete found the following unused dependencies in /proj/root/sub/Cargo.toml:\n",
            "\tunused_crate\n",
        );
        let d = AuditParser::MacheteText.parse(text, &decoder_root());
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].file, "Cargo.toml");
        assert_eq!(d[1].file, "sub/Cargo.toml");
        assert_eq!(d[1].message, "`unused_crate` — unused dependency");
    }

    /// No output (a clean run) → no findings.
    #[test]
    fn machete_text_clean_is_empty() {
        assert!(AuditParser::MacheteText.parse("", &decoder_root()).is_empty());
        assert!(AuditParser::MacheteText
            .parse("Analyzing your crate...\n", &decoder_root())
            .is_empty());
    }

    /// The two namespaces cannot collide: a plugin key always carries `@` and
    /// `/`, and no built-in wire id does. Nothing downstream has to
    /// disambiguate them, which is why nothing downstream does.
    #[test]
    fn a_plugin_key_can_never_collide_with_a_builtin_wire_id() {
        let set = crate::plugins::builtin::plugin_set();
        let ids: Vec<&str> = set
            .plugins
            .iter()
            .flat_map(|p| p.manifest.tools.iter())
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ids.len(), 14, "the built-in roster is fourteen tools");
        for id in ids {
            let key = ToolKey::Builtin(id.to_string());
            assert!(
                !key.wire().contains('@') && !key.wire().contains('/'),
                "built-in wire id `{id}` entered the plugin key namespace"
            );
            // The wire form is the bare id, byte-identical to what
            // `AuditToolId` serialized to before V38 — which is what keeps
            // every stored report, settings file and TS mirror valid.
            assert_eq!(
                serde_json::to_value(&key).unwrap(),
                serde_json::Value::String(id.to_string())
            );
            assert!(key.is_builtin(id));
        }
        let plugin = ToolKey::Plugin("acme@1.0.0/scan".to_string());
        assert_eq!(plugin.wire(), "acme@1.0.0/scan");
        assert!(!plugin.is_builtin("scan"));
    }

    /// The join, end to end: kind decides the umbrella, the registry's layered
    /// variables reach argv, and the user's parameters are APPENDED after the
    /// template — the `extra_args` contract, unchanged.
    #[test]
    fn from_effective_maps_the_registry_entry_onto_a_runnable_tool() {
        let dir = std::env::temp_dir().join(format!("cimp-runnable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "2.0.0",
              "categories": [{ "id": "q", "label": "Quality", "tools": ["lint"] }],
              "tools": [{
                "id": "lint", "label": "Acme Lint", "kind": "audit",
                "argv": ["--rules", "{var:ruleset}", "{root}"],
                "variables": [{ "name": "ruleset", "label": "Ruleset", "default": "p/default" }],
                "parameters_allowed": true,
                "findings_exit_codes": [1, 2],
                "timeout_secs": 300
              }]
            }"#,
        )
        .expect("write manifest");
        let set = crate::plugins::loader::scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);

        let mut cfg = crate::settings::ToolPluginsSettings::default();
        cfg.global_paths
            .insert("acme@2.0.0/lint".to_string(), r"C:\bin\acme.exe".to_string());
        cfg.plugins.insert(
            "acme@2.0.0".to_string(),
            crate::settings::PluginState {
                enabled: true,
                tools: std::collections::BTreeMap::from([(
                    "lint".to_string(),
                    crate::settings::ToolState {
                        variables: std::collections::BTreeMap::from([(
                            "ruleset".to_string(),
                            "p/ci".to_string(),
                        )]),
                        parameters: vec!["--exclude".into(), "vendor".into()],
                        ..Default::default()
                    },
                )]),
            },
        );
        let tools = crate::plugins::registry::runnable_tools(&set, &cfg, None);
        let runnable = RunnableAudit::from_effective(&tools[0])
            .expect("a valid audit tool")
            .expect("an umbrella tool");

        assert_eq!(runnable.category, Category::Quality, "audit kind ⇒ Quality");
        assert_eq!(runnable.key.wire(), "acme@2.0.0/lint");
        assert_eq!(runnable.program, r"C:\bin\acme.exe");
        assert_eq!(runnable.timeout_secs, Some(300));
        assert_eq!(runnable.findings_exit_codes, vec![1, 2]);
        assert_eq!(runnable.parser, AuditParser::Sarif);
        assert_eq!(runnable.transport, Transport::Stdout);
        assert_eq!(
            runnable.full_argv(Path::new(r"C:\proj"), None, true),
            vec!["--rules", "p/ci", r"C:\proj", "--exclude", "vendor"],
            "the user's value substitutes, and parameters land AFTER the template"
        );
        // A tool that declared no `dir_argv` is indifferent to whether the root
        // is a git repository — the same answer the pre-V38 adapter table gave.
        assert_eq!(
            runnable.full_argv(Path::new(r"C:\proj"), None, false),
            runnable.full_argv(Path::new(r"C:\proj"), None, true)
        );
        // A user plugin never resolves by name: no path, nothing runs.
        assert!(runnable.command.is_none());
        assert_eq!(runnable.spawn_subject(), "acme@2.0.0/lint");
        // Nothing declared `ingest`, so the strict kind-appropriate gate applies.
        assert_eq!(runnable.gate, IngestGate::Sarif);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest naming a diagnostics parser on a findings tool is refused,
    /// not defaulted: the wrong decoder returns zero findings, which is
    /// indistinguishable from a clean scan.
    #[test]
    fn a_diagnostics_parser_is_refused_on_a_findings_tool() {
        use crate::checks::ParserKind;
        assert_eq!(findings_parser(None).unwrap(), AuditParser::Sarif);
        assert_eq!(
            findings_parser(Some(ManifestParser::SARIF)).unwrap(),
            AuditParser::Sarif
        );
        assert_eq!(
            findings_parser(Some(ManifestParser::Legacy(LegacyAuditParser::KnipJson))).unwrap(),
            AuditParser::KnipJson
        );
        let e = findings_parser(Some(ManifestParser::Kind(ParserKind::CargoJson)))
            .expect_err("a diagnostics parser is not a findings parser");
        assert!(e.contains("cargo-json"), "{e}");
    }

    /// The manifest's runtime enum selects a sandbox profile by id, and `auto`
    /// / `none` are the two answers that select no row at all.
    #[test]
    fn the_runtime_declaration_maps_onto_the_sandbox_selection() {
        use crate::sandbox::RuntimeSelect;
        let mk = |r: RuntimeReq| {
            let mut t = fixture();
            t.runtime = r;
            t.runtime_select()
        };
        assert_eq!(mk(RuntimeReq::Auto), RuntimeSelect::Infer);
        assert_eq!(mk(RuntimeReq::None), RuntimeSelect::None);
        assert_eq!(mk(RuntimeReq::Python), RuntimeSelect::Profile("python"));
        assert_eq!(mk(RuntimeReq::Dotnet), RuntimeSelect::Profile("dotnet"));
    }

    fn fixture() -> RunnableAudit {
        RunnableAudit {
            key: ToolKey::Plugin("acme@1.0.0/scan".to_string()),
            label: "Acme Scan".to_string(),
            category: Category::Security,
            program: "C:\\bin\\acme.exe".to_string(),
            command: None,
            project_local_bin: None,
            argv: vec!["{root}".to_string()],
            dir_argv: Vec::new(),
            variables: BTreeMap::new(),
            parameters: Vec::new(),
            transport: Transport::Stdout,
            env: Vec::new(),
            findings_exit_codes: vec![1],
            timeout_secs: None,
            parser: AuditParser::Sarif,
            gate: IngestGate::Sarif,
            runtime: RuntimeReq::Auto,
            sandbox: SandboxReq::Required,
            extra_grants: Vec::new(),
            provenance: Provenance::User,
            applicability: manifest::Applicability::default(),
            provider: None,
        }
    }

    /// Applicability is the SAME test the built-in adapters get — no gate means
    /// always, and either list matching is enough.
    #[test]
    fn applicability_matches_the_builtin_rule() {
        let mut t = fixture();
        assert!(t.applicable(&Census::from_parts(&[], &[])));

        t.applicability = manifest::Applicability {
            extensions: vec!["java".to_string()],
            markers: vec!["pom.xml".to_string()],
        };
        assert!(!t.applicable(&Census::from_parts(&["rs"], &[])));
        assert!(t.applicable(&Census::from_parts(&["java"], &[])));
        assert!(t.applicable(&Census::from_parts(&[], &["pom.xml"])));
    }
}

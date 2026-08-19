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
use super::parsers::AuditParser;
use crate::plugins::manifest::{
    self, LegacyAuditParser, ManifestParser, Provenance, RuntimeReq, SandboxReq, ToolKind,
};
use crate::plugins::registry::EffectiveTool;
use crate::settings::AuditToolId;

/// Which tool a `ToolState` / `AuditFinding` belongs to.
///
/// A plugin's key always contains `@` and `/`; a built-in's kebab id never
/// does — so the two namespaces cannot collide, and a plugin can never present
/// itself to the UI, the report or the filter bar AS a built-in. That is not a
/// coincidence to rely on quietly: it is asserted by
/// `a_plugin_key_can_never_collide_with_a_builtin_wire_id`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolKey {
    /// One of the 14 built-in adapters.
    Builtin(AuditToolId),
    /// A plugin tool, by `plugins::loader::LoadedPlugin::tool_key`.
    Plugin(String),
}

impl ToolKey {
    /// The wire string — for a built-in, EXACTLY what `AuditToolId` serialized
    /// to before V38 (`osv-scanner`, `semgrep-quality`), because the Code Audit
    /// view, the report and the settings all key off it.
    pub fn wire(&self) -> String {
        match self {
            ToolKey::Builtin(id) => serde_json::to_value(id)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| id.command_name().to_string()),
            ToolKey::Plugin(key) => key.clone(),
        }
    }

    /// The built-in id, when this is one. The security floor and the
    /// no-shadowing tests ask this; nothing else may branch on it, which is
    /// why it is test-only until something outside a test legitimately needs
    /// to (Phase E's migration is the candidate).
    #[cfg(test)]
    pub fn builtin(&self) -> Option<AuditToolId> {
        match self {
            ToolKey::Builtin(id) => Some(*id),
            ToolKey::Plugin(_) => None,
        }
    }
}

impl PartialEq<AuditToolId> for ToolKey {
    /// So a caller that legitimately asks "is this chip the built-in gitleaks?"
    /// can, without unwrapping. A plugin key never equals a built-in id — which
    /// is the answer the security floor and the no-shadowing rule both want.
    fn eq(&self, other: &AuditToolId) -> bool {
        matches!(self, ToolKey::Builtin(id) if id == other)
    }
}

impl From<AuditToolId> for ToolKey {
    /// So a built-in call site still reads `ToolState::fresh(id, category)` —
    /// the 14 adapters are keyed by their enum everywhere they are declared,
    /// and only the RUNNER needs the widened identity.
    fn from(id: AuditToolId) -> Self {
        ToolKey::Builtin(id)
    }
}

impl Serialize for ToolKey {
    /// Delegates to `AuditToolId`'s own serializer for a built-in rather than
    /// re-spelling its kebab ids here — a second spelling is a second thing to
    /// keep in step with the TS mirror.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ToolKey::Builtin(id) => id.serialize(s),
            ToolKey::Plugin(key) => s.serialize_str(key),
        }
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
    /// The configured binary path, verbatim. Never resolved from PATH:
    /// decision 7 — cImp never picks a binary for a plugin.
    pub program: String,
    /// The manifest's argv template, tokens unsubstituted.
    pub argv: Vec<String>,
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
        let Some(program) = tool.path.clone() else {
            // `runnable_tools` filters these out; a caller that did not is
            // asking for a spawn with no program, which is not a tool error to
            // report but a caller bug to refuse.
            return Err("no binary path is configured".to_string());
        };
        let parser = findings_parser(tool.manifest.parser)?;
        Ok(Some(Self {
            key: ToolKey::Plugin(tool.tool_key.clone()),
            label: tool.manifest.label.clone(),
            category,
            program,
            argv: tool.manifest.argv.clone(),
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
            runtime: tool.manifest.runtime,
            sandbox: tool.manifest.sandbox,
            extra_grants: tool.manifest.extra_grants.clone(),
            provenance: tool.provenance,
            applicability: tool.manifest.applicability.clone(),
        }))
    }

    /// The same test [`super::adapters::Adapter::applicable`] applies: no gate
    /// = always applicable, else ANY listed extension OR ANY listed marker.
    /// One rule, two populations — a plugin tool must not be able to be gated
    /// differently from a built-in one.
    pub fn applicable(&self, census: &Census) -> bool {
        let a = &self.applicability;
        if a.extensions.is_empty() && a.markers.is_empty() {
            return true;
        }
        a.extensions.iter().any(|e| census.has_extension(e))
            || a.markers.iter().any(|m| census.has_marker(m))
    }

    /// The full argv: the substituted template, then the user's parameters
    /// appended verbatim (the `extra_args` contract).
    pub fn full_argv(&self, root: &Path, report: Option<&Path>) -> Vec<String> {
        let mut argv = render_argv(&self.argv, &self.variables, root, report);
        argv.extend(self.parameters.iter().cloned());
        argv
    }

    /// Which sandbox runtime profile this tool's grants come from.
    pub fn runtime_select(&self) -> crate::sandbox::RuntimeSelect {
        match self.runtime {
            RuntimeReq::Auto => crate::sandbox::RuntimeSelect::Infer,
            RuntimeReq::None => crate::sandbox::RuntimeSelect::None,
            // The ids are the same strings on both sides, pinned by Phase A's
            // `every_declared_runtime_names_a_real_sandbox_profile`.
            other => crate::sandbox::RuntimeSelect::Profile(other.as_str()),
        }
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

    /// The two namespaces cannot collide: a plugin key always carries `@` and
    /// `/`, and no built-in wire id does. Nothing downstream has to
    /// disambiguate them, which is why nothing downstream does.
    #[test]
    fn a_plugin_key_can_never_collide_with_a_builtin_wire_id() {
        for id in crate::settings::default_audit_tools() {
            let wire = ToolKey::Builtin(id.id).wire();
            assert!(
                !wire.contains('@') && !wire.contains('/'),
                "built-in wire id `{wire}` entered the plugin key namespace"
            );
            assert_eq!(
                serde_json::to_value(ToolKey::Builtin(id.id)).unwrap(),
                serde_json::to_value(id.id).unwrap(),
                "a built-in's wire form must be byte-identical to the pre-V38 one"
            );
        }
        let plugin = ToolKey::Plugin("acme@1.0.0/scan".to_string());
        assert_eq!(plugin.wire(), "acme@1.0.0/scan");
        assert_eq!(plugin.builtin(), None);
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
            argv: vec!["{root}".to_string()],
            variables: BTreeMap::new(),
            parameters: Vec::new(),
            transport: Transport::Stdout,
            env: Vec::new(),
            findings_exit_codes: vec![1],
            timeout_secs: None,
            parser: AuditParser::Sarif,
            runtime: RuntimeReq::Auto,
            sandbox: SandboxReq::Required,
            extra_grants: Vec::new(),
            provenance: Provenance::User,
            applicability: manifest::Applicability::default(),
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

//! V25 Phase B — the **audit-local parser dispatch**, now a shim.
//!
//! V38 Phase C emptied this file of decoders. Every [`AuditParser`] arm
//! delegates to [`crate::checks::parsers::parse`], because decision 3 wants ONE
//! parse boundary and the plugin population reaches it by `ParserKind` name:
//! two dispatch tables over the same decoders would be two places to register
//! the next one, and only one of them would get it.
//!
//! What survives here is the findings-side *namespace* (G2): on an
//! audit/security-kind tool a `parser` value names a findings decoder, and this
//! enum is that namespace. It is still the type each `static Adapter` carries,
//! so it retires with the adapter table itself in Phase E — the built-ins are
//! deliberately NOT migrated off it here (R4).
//!
//! The two behaviours that are this layer's own and not the checks layer's are
//! kept visible in [`AuditParser::parse`]: the audit pipeline presents
//! project-relative paths, so the eslint arm relativizes what the shared
//! decoder leaves absolute.

use std::path::Path;

use crate::checks::parsers::relativize;
use crate::checks::{Diag, ParserKind};

/// Which decoder turns a quality tool's captured output into [`Diag`]s. Carried
/// on each [`super::adapters::Adapter`]; the runner dispatches through
/// [`AuditParser::parse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditParser {
    /// SARIF 2.1 — delegates to the shared [`crate::checks::parsers`] parser
    /// (oxlint, golangci-lint, ruff, cppcheck, PMD, dotnet-analyzers,
    /// semgrep-quality).
    Sarif,
    /// ESLint `--format json` — delegates to the shared eslint parser, then
    /// relativizes its absolute `filePath`s against the scan root.
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
    /// malformed input yields an empty list, never an error.
    pub fn parse(self, output: &str, root: &Path) -> Vec<Diag> {
        match self {
            // The shared SARIF parser already relativizes via `sarif_uri_to_path`.
            AuditParser::Sarif => {
                crate::checks::parsers::parse(ParserKind::Sarif, output, "", root, None)
            }
            // Reuse the well-tested shared eslint decoder (severity 2→error,
            // 1→warning; ruleId→code), then relativize its absolute `filePath`s
            // (the audit pipeline presents project-relative paths; `checks::run`
            // does this relativization downstream, but the audit runner doesn't).
            AuditParser::EslintJson => {
                let mut diags =
                    crate::checks::parsers::parse(ParserKind::EslintJson, output, "", root, None);
                for d in &mut diags {
                    if !d.file.is_empty() {
                        d.file = relativize(root, &d.file);
                    }
                }
                diags
            }
            // Folded into `ParserKind` by R2 — the wire names `typos-jsonl`,
            // `knip-json` and `machete-text` name the same decoders a plugin
            // manifest (and, from Phase E, a built-in one) selects.
            AuditParser::TyposJsonl => {
                crate::checks::parsers::parse(ParserKind::TyposJsonl, output, "", root, None)
            }
            AuditParser::KnipJson => {
                crate::checks::parsers::parse(ParserKind::KnipJson, output, "", root, None)
            }
            AuditParser::MacheteText => {
                crate::checks::parsers::parse(ParserKind::MacheteText, output, "", root, None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Severity;
    use std::path::PathBuf;

    fn root() -> PathBuf {
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
        let d = AuditParser::Sarif.parse(SARIF, &root());
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
        let d = AuditParser::EslintJson.parse(json, &root());
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
        let d = AuditParser::TyposJsonl.parse(jsonl, &root());
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
        let d = AuditParser::TyposJsonl.parse(jsonl, &root());
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
        let d = AuditParser::KnipJson.parse(json, &root());
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
        assert!(AuditParser::KnipJson.parse("not json", &root()).is_empty());
        assert!(AuditParser::KnipJson
            .parse(r#"{"issues":[]}"#, &root())
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
        let d = AuditParser::MacheteText.parse(text, &root());
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
        let d = AuditParser::MacheteText.parse(text, &root());
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].file, "Cargo.toml");
        assert_eq!(d[1].file, "sub/Cargo.toml");
        assert_eq!(d[1].message, "`unused_crate` — unused dependency");
    }

    /// No output (a clean run) → no findings.
    #[test]
    fn machete_text_clean_is_empty() {
        assert!(AuditParser::MacheteText.parse("", &root()).is_empty());
        assert!(AuditParser::MacheteText
            .parse("Analyzing your crate...\n", &root())
            .is_empty());
    }
}

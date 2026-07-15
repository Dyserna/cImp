//! V25 Phase B — the **audit-local parser dispatch**.
//!
//! Most quality tools emit SARIF, which the runner already parses via the
//! shared [`crate::checks::parsers`] pipeline. The four tools that don't
//! (`typos` JSONL, ESLint/knip JSON, `cargo-machete` text) get a small,
//! fixture-tested parser here. Every parser is *lenient* — a line/record it
//! can't read is skipped, never an error (the `checks::parsers` posture) — and
//! clamps severities into the real `error | warning | note` set (V23 decision).
//! Paths are relativized against the scan `root` so a quality finding spells its
//! file exactly like a SARIF finding does.
//!
//! The dispatch ([`AuditParser::parse`]) is Phase B's deliverable; the Phase C
//! runner selects a tool's [`AuditParser`] from its adapter and feeds it the
//! tool's stdout (or report-file contents for a [`super::adapters::Transport::
//! ReportFile`] tool). Until then the entry point is exercised only by tests.

use std::path::Path;

use serde::Deserialize;

use crate::checks::parsers::relativize;
use crate::checks::{Diag, ParserKind, Severity};

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
            AuditParser::TyposJsonl => parse_typos_jsonl(output, root),
            AuditParser::KnipJson => parse_knip_json(output, root),
            AuditParser::MacheteText => parse_machete_text(output, root),
        }
    }
}

// ── typos --format json (TyposJsonl) ───────────────────────────────────────
//
// Web-verified 2026-07 against crate-ci/typos `crates/typos-cli/src/report.rs`
// + `crates/typos/src/dict.rs`: the reporter emits one JSON object per line
// with an internally-tagged `"type"` discriminator (snake_case). A misspelling
// is `"type": "typo"`, carrying a flattened `FileContext` (`path` + 1-based
// `line_num`), a 0-based `byte_offset`, the `typo` string, and `corrections`.
// `corrections` is `typos::Status` serialized `#[serde(untagged)]`: the
// `Corrections(Vec<String>)` variant becomes a JSON array of suggestion strings,
// while the unit `Valid`/`Invalid` variants become JSON `null` (a typo with no
// known correction) — hence `Option<Vec<String>>`. typos exits `2` when it
// finds typos (verified), which the adapter classifies as a findings code.

#[derive(Deserialize)]
struct TyposRecord {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    path: String,
    /// 1-based line number (flattened from `FileContext`); absent for a
    /// path/filename typo → 0.
    #[serde(default)]
    line_num: u32,
    /// 0-based byte offset within the line.
    #[serde(default)]
    byte_offset: u32,
    #[serde(default)]
    typo: String,
    /// `null` (no suggestion) or an array of corrections.
    #[serde(default)]
    corrections: Option<Vec<String>>,
}

fn parse_typos_jsonl(output: &str, root: &Path) -> Vec<Diag> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<TyposRecord>(line) else {
            continue;
        };
        if rec.kind != "typo" || rec.typo.is_empty() {
            continue;
        }
        let message = match rec.corrections.as_ref().filter(|c| !c.is_empty()) {
            Some(c) => format!("`{}` should be `{}`", rec.typo, c.join("` or `")),
            None => format!("`{}` is misspelled", rec.typo),
        };
        out.push(Diag {
            severity: Severity::Note,
            code: Some("typo".to_string()),
            message,
            file: relativize(root, &rec.path),
            line: rec.line_num,
            // typos reports a 0-based byte offset, not a column; surface it as a
            // 1-based positional hint (byte-accurate; approximate for multi-byte
            // characters, which typos itself doesn't disambiguate).
            col: Some(rec.byte_offset + 1),
        });
    }
    out
}

// ── knip --reporter json (KnipJson) ────────────────────────────────────────
//
// Web-verified 2026-07 against webpro-nl/knip `packages/knip/src/reporters/
// json.ts`: the reporter writes ONE JSON document `{ "issues": [entry, …] }` to
// stdout, each `entry` grouping one file's issues. `entry.file` is already
// relative to knip's cwd. Issue buckets are arrays of `{ name, line?, col?,
// … }`: `files` (the file itself is unused), `exports`, `types`, `dependencies`,
// `devDependencies`, `unlisted`, `unresolved`. knip exits `1` when it reports
// issues (verified). We surface unused files, exports/types, and the dependency
// buckets — all `warning`.

#[derive(Deserialize)]
struct KnipReport {
    #[serde(default)]
    issues: Vec<KnipEntry>,
}

#[derive(Deserialize, Default)]
struct KnipEntry {
    #[serde(default)]
    file: String,
    #[serde(default)]
    files: Vec<KnipItem>,
    #[serde(default)]
    exports: Vec<KnipItem>,
    #[serde(default)]
    types: Vec<KnipItem>,
    #[serde(default)]
    dependencies: Vec<KnipItem>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: Vec<KnipItem>,
    #[serde(default)]
    unlisted: Vec<KnipItem>,
    #[serde(default)]
    unresolved: Vec<KnipItem>,
}

#[derive(Deserialize, Default)]
struct KnipItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    line: u32,
    #[serde(default)]
    col: Option<u32>,
}

fn parse_knip_json(output: &str, root: &Path) -> Vec<Diag> {
    let doc = output.trim_start_matches('\u{feff}').trim();
    let Ok(report) = serde_json::from_str::<KnipReport>(doc) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in &report.issues {
        let file = relativize(root, &entry.file);
        // The whole file is unused (its `files` bucket is non-empty).
        if !entry.files.is_empty() {
            out.push(knip_diag(&file, "unused-file", "unused file".to_string(), 0, None));
        }
        for it in &entry.exports {
            out.push(knip_item_diag(&file, "unused-export", "unused export", it));
        }
        for it in &entry.types {
            out.push(knip_item_diag(&file, "unused-type", "unused type", it));
        }
        for it in &entry.dependencies {
            out.push(knip_item_diag(&file, "unused-dependency", "unused dependency", it));
        }
        for it in &entry.dev_dependencies {
            out.push(knip_item_diag(&file, "unused-dependency", "unused devDependency", it));
        }
        for it in &entry.unlisted {
            out.push(knip_item_diag(&file, "unlisted-dependency", "unlisted dependency", it));
        }
        for it in &entry.unresolved {
            out.push(knip_item_diag(&file, "unresolved", "unresolved import", it));
        }
    }
    out
}

/// A knip finding for a whole-file issue (no symbol).
fn knip_diag(file: &str, code: &str, message: String, line: u32, col: Option<u32>) -> Diag {
    Diag {
        severity: Severity::Warning,
        code: Some(code.to_string()),
        message,
        file: file.to_string(),
        line,
        col,
    }
}

/// A knip finding for a named item (`unused export `foo``), anchored to its
/// file and (when known) line/col.
fn knip_item_diag(file: &str, code: &str, label: &str, it: &KnipItem) -> Diag {
    let message = if it.name.is_empty() {
        label.to_string()
    } else {
        format!("{label} `{}`", it.name)
    };
    knip_diag(file, code, message, it.line, it.col)
}

// ── cargo-machete text (MacheteText) ───────────────────────────────────────
//
// Web-verified 2026-07 against bnjbvr/cargo-machete `src/main.rs`: on finding
// unused dependencies it prints to STDOUT a header
//   `cargo-machete found the following unused dependencies in <location>:`
// followed by one tab-indented crate name per line (`\t<dep>`). `<location>` is
// the crate directory / manifest. cargo-machete exits `1` when it finds unused
// dependencies, `2` on error (verified). The spec assumed a per-line
// `<crate> — unused dependency in <path>` shape; the real format is this
// header-then-indented-list, which this parser follows.

fn parse_machete_text(output: &str, root: &Path) -> Vec<Diag> {
    const HEADER_PREFIX: &str = "cargo-machete found the following unused dependencies in ";
    let mut out = Vec::new();
    // The Cargo.toml the current header's crates are anchored to.
    let mut anchor: Option<String> = None;
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix(HEADER_PREFIX) {
            let location = rest.trim_end().trim_end_matches(':').trim();
            anchor = Some(machete_manifest(root, location));
            continue;
        }
        // Crate names are tab/space-indented under the most recent header.
        let is_indented = line.starts_with('\t') || line.starts_with("    ");
        if !is_indented {
            continue;
        }
        let krate = line.trim();
        if krate.is_empty() {
            continue;
        }
        let Some(file) = anchor.clone() else { continue };
        out.push(Diag {
            severity: Severity::Warning,
            code: Some("unused-dependency".to_string()),
            message: format!("`{krate}` — unused dependency"),
            file,
            line: 0,
            col: None,
        });
    }
    out
}

/// Resolve cargo-machete's header `<location>` to the project-relative
/// `Cargo.toml` its crates live in: use it verbatim when it already points at a
/// manifest, else join `Cargo.toml`. Relativized against the scan root.
fn machete_manifest(root: &Path, location: &str) -> String {
    let normalized = location.replace('\\', "/");
    let rel = if normalized.to_ascii_lowercase().ends_with("cargo.toml") {
        relativize(root, &normalized)
    } else {
        let dir = relativize(root, normalized.trim_end_matches('/'));
        if dir.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{}/Cargo.toml", dir.trim_end_matches('/'))
        }
    };
    rel
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let export = d.iter().find(|x| x.code.as_deref() == Some("unused-export")).unwrap();
        assert_eq!(export.message, "unused export `factorial`");
        assert_eq!(export.file, "src/math.ts");
        assert_eq!(export.line, 12);
        assert_eq!(export.col, Some(14));
        let ty = d.iter().find(|x| x.code.as_deref() == Some("unused-type")).unwrap();
        assert_eq!(ty.message, "unused type `Radians`");
        let dep = d.iter().find(|x| x.code.as_deref() == Some("unused-dependency")).unwrap();
        assert_eq!(dep.message, "unused dependency `lodash`");
        assert_eq!(dep.file, "package.json");
        let unlisted = d.iter().find(|x| x.code.as_deref() == Some("unlisted-dependency")).unwrap();
        assert_eq!(unlisted.message, "unlisted dependency `rimraf`");
    }

    /// Malformed knip JSON yields no findings (module posture), never an error.
    #[test]
    fn knip_json_malformed_is_empty() {
        assert!(AuditParser::KnipJson.parse("not json", &root()).is_empty());
        assert!(AuditParser::KnipJson.parse(r#"{"issues":[]}"#, &root()).is_empty());
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
        assert!(AuditParser::MacheteText.parse("Analyzing your crate...\n", &root()).is_empty());
    }
}

//! V23 Phase B — the data-driven **audit adapter registry**.
//!
//! One [`Adapter`] per [`AuditToolId`] describes everything the runner needs to
//! invoke a security scanner and read its SARIF: the fixed argv template, where
//! the SARIF is delivered ([`Transport`]), any forced child env, and the
//! exit-code semantics. The table is intentionally data-driven so the deferred
//! GuardDog / Trivy tools become additional [`Adapter`] rows rather than new
//! control flow.
//!
//! **Exit-code semantics live here, not in `checks`.** V22's `run_check` treats
//! a non-zero exit as a checker *failure*; audit tools invert that — for all
//! three v1 tools `0` = clean, `1` = findings present (a SUCCESS), anything else
//! = a genuine tool error. [`Adapter::classify_exit`] owns that distinction.

use std::path::Path;

use super::census::Census;
use super::parsers::AuditParser;
use crate::settings::AuditToolId;

/// Which tab / section a tool belongs to. V25 keeps Security (the V23 trio) and
/// Quality (the V25 linters) in separate tabs with independent runs; the runner
/// filters a scan by this so a Quality scan never launches a Security tool.
///
/// Serializes lowercase (`"security"` / `"quality"`) — the wire string the
/// `audit_start_scan` command accepts and the `category` per-tool snapshot field
/// carries (mirrored in `src/lib/codeAudit/types.ts` as `AuditCategory`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Security,
    Quality,
}

/// The project shape a tool applies to: any file extension in `extensions`, OR
/// any marker token in `markers` (a [`super::census::MARKERS`] token). Both
/// empty = always applicable (the security trio, `typos`, `semgrep-quality`).
/// Static, so it costs nothing at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applicability {
    pub extensions: &'static [&'static str],
    pub markers: &'static [&'static str],
}

impl Applicability {
    /// The always-applicable shape (no gate).
    const ALWAYS: Applicability = Applicability {
        extensions: &[],
        markers: &[],
    };
}

/// Where a tool writes its SARIF report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// SARIF is emitted on the child's stdout (osv-scanner, semgrep).
    Stdout,
    /// SARIF is written to a temp report file whose path is substituted for the
    /// [`Arg::Report`] token in the argv; the child's stdout carries logs
    /// (gitleaks). The runner reads the file after the child exits.
    ReportFile,
}

/// One token in an adapter's fixed argv template. Kept symbolic (rather than a
/// pre-rendered `Vec<String>`) so the runner can substitute the live scan root
/// and temp report path at spawn time while the table stays `static`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    /// A literal subcommand or flag, passed through verbatim.
    Lit(&'static str),
    /// Substituted with the scan root (absolute path string).
    Root,
    /// Substituted with the temp SARIF report path ([`Transport::ReportFile`]
    /// only). Renders empty if the runner passes no report path.
    Report,
    /// A literal argv token with the substring `{report}` replaced by the temp
    /// report path — for a tool that embeds the report path *inside* a larger
    /// flag rather than as its own token (dotnet's
    /// `/p:ErrorLog={report},version=2.1`). `{report}` renders empty when the
    /// runner passes no report path. [`Transport::ReportFile`] only.
    ReportIn(&'static str),
    /// A registry ruleset slug (the value after a `--config`-style flag):
    /// renders the user's per-tool `ruleset` setting when non-empty, else this
    /// built-in default. Exists because registry slugs can vanish server-side
    /// without notice (`p/best-practices` 404'd 2026-07) — an override is then
    /// a settings edit, not a rebuild. Only the two semgrep adapters carry it.
    Ruleset(&'static str),
}

/// How a tool's exit code classifies once it has run to completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitClass {
    /// Exit 0 — the tool ran and reported nothing.
    Clean,
    /// A configured findings-present code (e.g. gitleaks/osv/semgrep `1`) — the
    /// tool ran *successfully* and its report carries findings. NOT an error.
    Findings,
    /// Any other code (or no code — a killed/crashed child) — a genuine tool
    /// error the runner surfaces as `failed`.
    Error,
}

/// A built-in audit tool adapter. `static`, so the registry costs nothing at
/// runtime; the per-scan variation (root, temp path, `extra_args`) is applied
/// by [`Adapter::full_argv`].
#[derive(Debug)]
pub struct Adapter {
    /// The fixed argv template. For a tool with a git/non-git split
    /// ([`dir_argv`](Self::dir_argv) set) this is the git-repo form.
    pub argv: &'static [Arg],
    /// Fallback argv template used when the scan root is *not* a git repo
    /// (gitleaks `dir` vs `git`). `None` = this tool is indifferent to whether
    /// the root is a git repo and always uses [`argv`](Self::argv).
    pub dir_argv: Option<&'static [Arg]>,
    /// Where this tool delivers its SARIF.
    pub transport: Transport,
    /// Environment forced onto the child (e.g. semgrep's `PYTHONUTF8=1`, needed
    /// for its beta Windows support).
    pub env: &'static [(&'static str, &'static str)],
    /// Non-zero exit codes that still mean "ran fine, here are findings". `0`
    /// is *always* [`ExitClass::Clean`]; a code in this set is
    /// [`ExitClass::Findings`]; anything else is [`ExitClass::Error`].
    pub findings_exit_codes: &'static [i32],
    /// Which tab/section this tool belongs to — a scan runs one [`Category`].
    /// Read by the Phase C runner's category filter.
    pub category: Category,
    /// The project shape this tool applies to. [`Applicability::ALWAYS`] = no
    /// gate. Consulted by [`applicable`](Self::applicable) against the census.
    pub applicability: Applicability,
    /// The decoder for this tool's output. SARIF tools delegate to the shared
    /// parser; the JSON/JSONL/text tools use an [`AuditParser`] variant. Read by
    /// the Phase C runner when it decodes findings.
    pub parser: AuditParser,
    /// For a node tool (eslint, knip), the `node_modules/.bin` shim name to
    /// prefer over a global install — resolved before ebin/PATH by
    /// [`super::resolve_audit_binary`]. `None` = no project-local resolution.
    pub project_local_bin: Option<&'static str>,
}

impl Adapter {
    /// Render the fixed argv (no `extra_args`) for a concrete scan: choose the
    /// git vs `dir` template, then substitute [`Arg::Root`] / [`Arg::Report`] /
    /// [`Arg::Ruleset`]. `ruleset` `None` (or empty via [`full_argv`]) keeps
    /// each [`Arg::Ruleset`] token's built-in default.
    fn render_argv(
        &self,
        root: &Path,
        report: Option<&Path>,
        git_repo: bool,
        ruleset: Option<&str>,
    ) -> Vec<String> {
        let template = match (self.dir_argv, git_repo) {
            (Some(dir), false) => dir,
            _ => self.argv,
        };
        template
            .iter()
            .map(|a| match a {
                Arg::Lit(s) => (*s).to_string(),
                Arg::Root => root.to_string_lossy().into_owned(),
                Arg::Report => report
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                Arg::ReportIn(tpl) => {
                    let path = report
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    tpl.replace("{report}", &path)
                }
                Arg::Ruleset(default) => ruleset.unwrap_or(default).to_string(),
            })
            .collect()
    }

    /// Render the fixed argv (no `extra_args`, built-in rulesets) for a
    /// concrete scan. Test-only convenience — the runner goes through
    /// [`full_argv`](Self::full_argv).
    #[cfg(test)]
    pub fn resolve_argv(&self, root: &Path, report: Option<&Path>, git_repo: bool) -> Vec<String> {
        self.render_argv(root, report, git_repo, None)
    }

    /// The full argv the runner spawns: the fixed template (with the user's
    /// `ruleset` substituted for any [`Arg::Ruleset`] token — empty keeps the
    /// built-in default) followed by the user's per-tool `extra_args`
    /// (appended verbatim, after the fixed argv — the settings contract).
    pub fn full_argv(
        &self,
        root: &Path,
        report: Option<&Path>,
        git_repo: bool,
        extra_args: &[String],
        ruleset: &str,
    ) -> Vec<String> {
        let mut argv = self.render_argv(
            root,
            report,
            git_repo,
            (!ruleset.is_empty()).then_some(ruleset),
        );
        argv.extend(extra_args.iter().cloned());
        argv
    }

    /// The built-in registry ruleset, if this adapter carries an
    /// [`Arg::Ruleset`] token (the two semgrep tools). `None` = the tool has
    /// no ruleset concept and the per-tool `ruleset` setting is ignored.
    /// Test-only — the Settings UI hardcodes the two defaults in its metadata.
    #[cfg(test)]
    pub fn default_ruleset(&self) -> Option<&'static str> {
        self.argv.iter().find_map(|a| match a {
            Arg::Ruleset(d) => Some(*d),
            _ => None,
        })
    }

    /// Whether this tool applies to a project with the given [`Census`]: always
    /// true when its [`Applicability`] is empty ([`Applicability::ALWAYS`]),
    /// else true if ANY listed extension OR ANY listed marker was seen.
    /// Called by the Phase C runner to skip a tool the project doesn't need.
    pub fn applicable(&self, census: &Census) -> bool {
        let a = &self.applicability;
        if a.extensions.is_empty() && a.markers.is_empty() {
            return true;
        }
        a.extensions.iter().any(|e| census.has_extension(e))
            || a.markers.iter().any(|m| census.has_marker(m))
    }

    /// Classify a completed child's exit code (`None` = killed / no code).
    pub fn classify_exit(&self, code: Option<i32>) -> ExitClass {
        match code {
            Some(0) => ExitClass::Clean,
            Some(c) if self.findings_exit_codes.contains(&c) => ExitClass::Findings,
            _ => ExitClass::Error,
        }
    }
}

// ── the v1 registry ────────────────────────────────────────────────────────

/// `osv-scanner scan source -r <root> --format sarif` → SARIF on stdout.
static OSV_SCANNER: Adapter = Adapter {
    argv: &[
        Arg::Lit("scan"),
        Arg::Lit("source"),
        Arg::Lit("-r"),
        Arg::Root,
        Arg::Lit("--format"),
        Arg::Lit("sarif"),
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Security,
    applicability: Applicability::ALWAYS,
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// `gitleaks git <root> --report-format sarif --report-path <tmp> --exit-code 1`
/// (SARIF read from the temp file; stdout is logs). Falls back to
/// `gitleaks dir <root> …` when `<root>` is not a git repo.
static GITLEAKS: Adapter = Adapter {
    argv: &[
        Arg::Lit("git"),
        Arg::Root,
        Arg::Lit("--report-format"),
        Arg::Lit("sarif"),
        Arg::Lit("--report-path"),
        Arg::Report,
        Arg::Lit("--exit-code"),
        Arg::Lit("1"),
    ],
    dir_argv: Some(&[
        Arg::Lit("dir"),
        Arg::Root,
        Arg::Lit("--report-format"),
        Arg::Lit("sarif"),
        Arg::Lit("--report-path"),
        Arg::Report,
        Arg::Lit("--exit-code"),
        Arg::Lit("1"),
    ]),
    transport: Transport::ReportFile,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Security,
    applicability: Applicability::ALWAYS,
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// `semgrep scan --config auto --sarif --quiet <root>` → SARIF on stdout, with
/// `PYTHONUTF8=1` forced (semgrep's beta Windows support mangles output
/// otherwise).
static SEMGREP: Adapter = Adapter {
    argv: &[
        Arg::Lit("scan"),
        Arg::Lit("--config"),
        Arg::Ruleset("auto"),
        Arg::Lit("--sarif"),
        Arg::Lit("--quiet"),
        Arg::Root,
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[("PYTHONUTF8", "1")],
    findings_exit_codes: &[1],
    category: Category::Security,
    applicability: Applicability::ALWAYS,
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

// ── the V25 Quality registry ────────────────────────────────────────────────
//
// Flags/exit-codes/output shapes below were web-verified against current
// official docs/source (2026-07); deviations from the V25 spec's assumptions
// are called out inline. Every Quality tool runs with `cwd = <root>` (the
// runner sets it), so linters that default to "lint the current directory"
// need no explicit root token.

/// oxlint — `oxlint --format sarif` → SARIF on stdout (verified: oxlint emits
/// SARIF v2.1.0 to stdout; exit 1 when diagnostics are present). Lints the cwd
/// tree by default, so no root token. JS/TS-family extensions gate it.
static OXLINT: Adapter = Adapter {
    argv: &[Arg::Lit("--format"), Arg::Lit("sarif")],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &["js", "ts", "jsx", "tsx", "mjs", "cjs"],
        markers: &[],
    },
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// golangci-lint (v2) — `golangci-lint run --output.sarif.path stdout` → SARIF
/// on stdout (verified: v2 replaced `--out-format` with `--output.*`;
/// `--output.sarif.path` accepts `stdout`; exit 1 on issues). `run` defaults to
/// `./...` under cwd. Gated on `go.mod` or `.go` files.
static GOLANGCI_LINT: Adapter = Adapter {
    argv: &[
        Arg::Lit("run"),
        Arg::Lit("--output.sarif.path"),
        Arg::Lit("stdout"),
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &["go"],
        markers: &["go.mod"],
    },
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// ruff — `ruff check --output-format sarif` → SARIF on stdout (verified: ruff
/// supports `--output-format sarif`; exit 1 when violations found). Checks the
/// cwd tree by default. Gated on `.py` files.
static RUFF: Adapter = Adapter {
    argv: &[
        Arg::Lit("check"),
        Arg::Lit("--output-format"),
        Arg::Lit("sarif"),
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &["py"],
        markers: &[],
    },
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// cppcheck (≥ 2.16) — `cppcheck --enable=warning,style --output-format=sarif
/// --output-file=<tmp> <root>`.
///
/// **Spec deviation (web-verified 2026-07):** the spec assumed SARIF on *stdout*
/// with `Transport::Stdout`. cppcheck writes analysis results to **stderr**, not
/// stdout, so a stdout capture would silently read nothing. cppcheck ≥ 2.16
/// supports `--output-file=<file>`, which writes the (SARIF-formatted) report to
/// a file — so this adapter uses [`Transport::ReportFile`] (like gitleaks)
/// instead, sidestepping the stdout/stderr ambiguity entirely. Exit code is
/// **0 even with findings** unless `--error-exitcode` is set (verified) — we
/// don't set it, so `findings_exit_codes` is empty and a clean exit-0 run with a
/// populated report is the normal "findings present" path. Gated on C/C++ exts.
static CPPCHECK: Adapter = Adapter {
    argv: &[
        Arg::Lit("--enable=warning,style"),
        Arg::Lit("--output-format=sarif"),
        Arg::ReportIn("--output-file={report}"),
        Arg::Root,
    ],
    dir_argv: None,
    transport: Transport::ReportFile,
    env: &[],
    // Exit 0 even with findings (no `--error-exitcode`); the report carries them.
    findings_exit_codes: &[],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &["c", "cc", "cpp", "cxx", "h", "hpp"],
        markers: &[],
    },
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// typos — `typos --format json` → JSONL on stdout, one record per line
/// (verified: internally-tagged `type` discriminator; exit **2** when typos are
/// found). Always applicable — a spell checker is valuable on every project.
static TYPOS: Adapter = Adapter {
    argv: &[Arg::Lit("--format"), Arg::Lit("json"), Arg::Root],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[2],
    category: Category::Quality,
    applicability: Applicability::ALWAYS,
    parser: AuditParser::TyposJsonl,
    project_local_bin: None,
};

/// ESLint — `eslint --format json .` → JSON on stdout (exit 1 when lint errors
/// present). JSON (not SARIF) avoids requiring the `@microsoft/eslint-formatter-
/// sarif` package in the target project. Resolved project-local-first
/// (`node_modules/.bin/eslint`). Gated on an eslint config marker.
static ESLINT: Adapter = Adapter {
    argv: &[Arg::Lit("--format"), Arg::Lit("json"), Arg::Root],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &[],
        markers: &["eslint.config", ".eslintrc"],
    },
    parser: AuditParser::EslintJson,
    project_local_bin: Some("eslint"),
};

/// PMD — `pmd check -d <root> -R rulesets/java/quickstart.xml -f sarif` → SARIF
/// on stdout (verified: `check` subcommand; `-d`/`-R`/`-f`; `sarif` is a valid
/// format; exit **4** when violations found, **5** on recoverable error; Windows
/// launcher is `pmd.bat`, resolved via `pty::resolve`'s `.bat` trial). Gated on
/// `.java` files. We keep the default fail-on-violation exit (4) rather than
/// `--no-fail-on-violation` so a tool *error* stays distinguishable from
/// findings.
static PMD: Adapter = Adapter {
    argv: &[
        Arg::Lit("check"),
        Arg::Lit("-d"),
        Arg::Root,
        Arg::Lit("-R"),
        Arg::Lit("rulesets/java/quickstart.xml"),
        Arg::Lit("-f"),
        Arg::Lit("sarif"),
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[4],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &["java"],
        markers: &[],
    },
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// Roslyn analyzers via the .NET SDK — `dotnet build
/// /p:ErrorLog=<tmp>,version=2.1 -nologo` → SARIF written to the report file
/// (exit 1 on build/analyzer errors). **Default-disabled** in settings: this
/// runs a real build (restores packages, writes obj/bin). The `/p:ErrorLog`
/// value embeds the report path inside one MSBuild-property token, hence
/// [`Arg::ReportIn`]. Gated on `*.sln` / `*.csproj`.
static DOTNET_ANALYZERS: Adapter = Adapter {
    argv: &[
        Arg::Lit("build"),
        Arg::ReportIn("/p:ErrorLog={report},version=2.1"),
        Arg::Lit("-nologo"),
    ],
    dir_argv: None,
    transport: Transport::ReportFile,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &[],
        markers: &["*.sln", "*.csproj"],
    },
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// knip — `knip --reporter json` → one JSON document on stdout (verified:
/// `{ issues: [...] }`; exit 1 when issues reported). Resolved
/// project-local-first (`node_modules/.bin/knip`). Gated on `package.json`.
static KNIP: Adapter = Adapter {
    argv: &[Arg::Lit("--reporter"), Arg::Lit("json")],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &[],
        markers: &["package.json"],
    },
    parser: AuditParser::KnipJson,
    project_local_bin: Some("knip"),
};

/// cargo-machete — text output on stdout (verified: header line + tab-indented
/// crate names; exit **1** when unused deps found, **2** on error). Run with no
/// path arg so it analyzes `cwd` (= root) and its sub-crates; avoids the cargo
/// subcommand's `machete` arg-stripping ambiguity. Gated on `Cargo.toml`.
static CARGO_MACHETE: Adapter = Adapter {
    argv: &[],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability {
        extensions: &[],
        markers: &["Cargo.toml"],
    },
    parser: AuditParser::MacheteText,
    project_local_bin: None,
};

/// semgrep with a quality ruleset — same shape as the Security [`SEMGREP`] but
/// `--config p/r2c-best-practices` (exit 1 on findings). Separate id so quality
/// rules never appear in the Security section. **Default-disabled** (its
/// registry ruleset needs network). Always applicable.
/// `p/best-practices` vanished from the registry (HTTP 404 as of 2026-07-17,
/// semgrep exit 7); the older-named `p/r2c-best-practices` pack still resolves.
static SEMGREP_QUALITY: Adapter = Adapter {
    argv: &[
        Arg::Lit("scan"),
        Arg::Lit("--config"),
        Arg::Ruleset("p/r2c-best-practices"),
        Arg::Lit("--sarif"),
        Arg::Lit("--quiet"),
        Arg::Root,
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[("PYTHONUTF8", "1")],
    findings_exit_codes: &[1],
    category: Category::Quality,
    applicability: Applicability::ALWAYS,
    parser: AuditParser::Sarif,
    project_local_bin: None,
};

/// The built-in adapter for `id`. Total over the closed [`AuditToolId`] enum.
pub fn adapter(id: AuditToolId) -> &'static Adapter {
    match id {
        AuditToolId::OsvScanner => &OSV_SCANNER,
        AuditToolId::Gitleaks => &GITLEAKS,
        AuditToolId::Semgrep => &SEMGREP,
        AuditToolId::Oxlint => &OXLINT,
        AuditToolId::GolangciLint => &GOLANGCI_LINT,
        AuditToolId::Ruff => &RUFF,
        AuditToolId::Cppcheck => &CPPCHECK,
        AuditToolId::Typos => &TYPOS,
        AuditToolId::Eslint => &ESLINT,
        AuditToolId::Pmd => &PMD,
        AuditToolId::DotnetAnalyzers => &DOTNET_ANALYZERS,
        AuditToolId::Knip => &KNIP,
        AuditToolId::CargoMachete => &CARGO_MACHETE,
        AuditToolId::SemgrepQuality => &SEMGREP_QUALITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/proj/root")
    }

    #[test]
    fn osv_argv_substitutes_root_and_appends_extra_args() {
        let a = adapter(AuditToolId::OsvScanner);
        let argv = a.full_argv(&root(), None, true, &["--offline".to_string()], "");
        assert_eq!(
            argv,
            vec![
                "scan",
                "source",
                "-r",
                "/proj/root",
                "--format",
                "sarif",
                "--offline",
            ]
        );
        assert_eq!(a.transport, Transport::Stdout);
    }

    #[test]
    fn gitleaks_git_form_substitutes_root_and_report() {
        let a = adapter(AuditToolId::Gitleaks);
        let report = PathBuf::from("/tmp/gl.sarif");
        let argv = a.full_argv(&root(), Some(&report), true, &[], "");
        assert_eq!(
            argv,
            vec![
                "git",
                "/proj/root",
                "--report-format",
                "sarif",
                "--report-path",
                "/tmp/gl.sarif",
                "--exit-code",
                "1",
            ]
        );
        assert_eq!(a.transport, Transport::ReportFile);
    }

    #[test]
    fn gitleaks_falls_back_to_dir_when_not_a_git_repo() {
        let a = adapter(AuditToolId::Gitleaks);
        let report = PathBuf::from("/tmp/gl.sarif");
        let argv = a.resolve_argv(&root(), Some(&report), false);
        assert_eq!(argv[0], "dir");
        assert_eq!(argv[1], "/proj/root");
        // git form is still selected when it IS a git repo.
        assert_eq!(a.resolve_argv(&root(), Some(&report), true)[0], "git");
    }

    #[test]
    fn semgrep_argv_and_forced_utf8_env() {
        let a = adapter(AuditToolId::Semgrep);
        let argv = a.full_argv(&root(), None, false, &["--config".into(), "p/ci".into()], "");
        assert_eq!(
            argv,
            vec![
                "scan",
                "--config",
                "auto",
                "--sarif",
                "--quiet",
                "/proj/root",
                "--config",
                "p/ci",
            ]
        );
        assert_eq!(a.env, &[("PYTHONUTF8", "1")]);
    }

    #[test]
    fn override_path_is_orthogonal_to_argv() {
        // The resolved binary path is the *program*; argv never carries it, so a
        // per-tool `path` override changes nothing about argv construction.
        let a = adapter(AuditToolId::OsvScanner);
        let argv = a.resolve_argv(&root(), None, true);
        assert!(!argv.iter().any(|s| s.contains("osv-scanner")));
    }

    #[test]
    fn security_trio_is_always_applicable() {
        // The V23 trio has no gate — applicable even against an empty census.
        let empty = Census::default();
        for id in [
            AuditToolId::OsvScanner,
            AuditToolId::Gitleaks,
            AuditToolId::Semgrep,
        ] {
            let a = adapter(id);
            assert_eq!(a.category, Category::Security, "{id:?}");
            assert_eq!(a.applicability, Applicability::ALWAYS, "{id:?}");
            assert!(a.applicable(&empty), "{id:?} always applicable");
        }
    }

    /// The applicability logic (extension gate OR marker gate) via constructed
    /// adapters — Phase B's real quality tools reuse this exact predicate.
    #[test]
    fn applicable_honors_extension_and_marker_gates() {
        let ext_gated = Adapter {
            argv: &[],
            dir_argv: None,
            transport: Transport::Stdout,
            env: &[],
            findings_exit_codes: &[1],
            category: Category::Quality,
            applicability: Applicability {
                extensions: &["go"],
                markers: &[],
            },
            parser: AuditParser::Sarif,
            project_local_bin: None,
        };
        assert!(ext_gated.applicable(&Census::from_parts(&["go", "rs"], &[])));
        assert!(!ext_gated.applicable(&Census::from_parts(&["rs"], &[])));
        assert!(!ext_gated.applicable(&Census::default()));

        let marker_gated = Adapter {
            applicability: Applicability {
                extensions: &[],
                markers: &["go.mod"],
            },
            ..ext_gated_shape()
        };
        assert!(marker_gated.applicable(&Census::from_parts(&[], &["go.mod"])));
        assert!(!marker_gated.applicable(&Census::from_parts(&[], &["Cargo.toml"])));

        // Either gate is sufficient (OR, not AND): the eslint-shaped tool with
        // two markers matches on the legacy one alone.
        let eslint_shape = Adapter {
            applicability: Applicability {
                extensions: &[],
                markers: &["eslint.config", ".eslintrc"],
            },
            ..ext_gated_shape()
        };
        assert!(eslint_shape.applicable(&Census::from_parts(&[], &[".eslintrc"])));
        assert!(!eslint_shape.applicable(&Census::from_parts(&[], &["package.json"])));

        // Empty applicability is always applicable, even on an empty census.
        let always = Adapter {
            applicability: Applicability::ALWAYS,
            ..ext_gated_shape()
        };
        assert!(always.applicable(&Census::default()));
    }

    /// A minimal Quality-category adapter shell for the applicability tests to
    /// override `applicability` on (avoids repeating every field).
    fn ext_gated_shape() -> Adapter {
        Adapter {
            argv: &[],
            dir_argv: None,
            transport: Transport::Stdout,
            env: &[],
            findings_exit_codes: &[1],
            category: Category::Quality,
            applicability: Applicability::ALWAYS,
            parser: AuditParser::Sarif,
            project_local_bin: None,
        }
    }

    #[test]
    fn exit_code_classification_per_tool() {
        for id in [
            AuditToolId::OsvScanner,
            AuditToolId::Gitleaks,
            AuditToolId::Semgrep,
        ] {
            let a = adapter(id);
            assert_eq!(a.classify_exit(Some(0)), ExitClass::Clean, "{id:?} 0");
            assert_eq!(a.classify_exit(Some(1)), ExitClass::Findings, "{id:?} 1");
            assert_eq!(a.classify_exit(Some(2)), ExitClass::Error, "{id:?} 2");
            assert_eq!(a.classify_exit(Some(127)), ExitClass::Error, "{id:?} 127");
            // A killed child (no exit code) is a tool error, never findings.
            assert_eq!(a.classify_exit(None), ExitClass::Error, "{id:?} none");
        }
    }

    // ── V25 Quality adapters ────────────────────────────────────────────────

    #[test]
    fn quality_stdout_argvs_substitute_and_append() {
        // oxlint / ruff / golangci-lint: fixed flags, cwd-relative (no root),
        // extra args appended.
        let ox = adapter(AuditToolId::Oxlint);
        assert_eq!(
            ox.full_argv(&root(), None, true, &["--deny-warnings".into()], ""),
            vec!["--format", "sarif", "--deny-warnings"]
        );
        assert_eq!(ox.transport, Transport::Stdout);
        assert_eq!(ox.parser, AuditParser::Sarif);

        let ruff = adapter(AuditToolId::Ruff);
        assert_eq!(
            ruff.resolve_argv(&root(), None, true),
            vec!["check", "--output-format", "sarif"]
        );

        let gcl = adapter(AuditToolId::GolangciLint);
        assert_eq!(
            gcl.resolve_argv(&root(), None, true),
            vec!["run", "--output.sarif.path", "stdout"]
        );

        // typos passes the root and uses the JSONL parser.
        let typos = adapter(AuditToolId::Typos);
        assert_eq!(
            typos.resolve_argv(&root(), None, true),
            vec!["--format", "json", "/proj/root"]
        );
        assert_eq!(typos.parser, AuditParser::TyposJsonl);
    }

    #[test]
    fn eslint_and_knip_are_project_local_and_json() {
        let eslint = adapter(AuditToolId::Eslint);
        assert_eq!(eslint.project_local_bin, Some("eslint"));
        assert_eq!(eslint.parser, AuditParser::EslintJson);
        assert_eq!(
            eslint.resolve_argv(&root(), None, true),
            vec!["--format", "json", "/proj/root"]
        );

        let knip = adapter(AuditToolId::Knip);
        assert_eq!(knip.project_local_bin, Some("knip"));
        assert_eq!(knip.parser, AuditParser::KnipJson);
        assert_eq!(
            knip.resolve_argv(&root(), None, true),
            vec!["--reporter", "json"]
        );
    }

    #[test]
    fn pmd_argv_substitutes_root() {
        let pmd = adapter(AuditToolId::Pmd);
        assert_eq!(
            pmd.resolve_argv(&root(), None, true),
            vec![
                "check",
                "-d",
                "/proj/root",
                "-R",
                "rulesets/java/quickstart.xml",
                "-f",
                "sarif"
            ]
        );
    }

    #[test]
    fn cppcheck_uses_report_file_and_embeds_path() {
        let a = adapter(AuditToolId::Cppcheck);
        assert_eq!(a.transport, Transport::ReportFile);
        let report = PathBuf::from("/tmp/cc.sarif");
        assert_eq!(
            a.resolve_argv(&root(), Some(&report), true),
            vec![
                "--enable=warning,style",
                "--output-format=sarif",
                "--output-file=/tmp/cc.sarif",
                "/proj/root",
            ]
        );
    }

    #[test]
    fn dotnet_embeds_report_path_in_msbuild_property() {
        let a = adapter(AuditToolId::DotnetAnalyzers);
        assert_eq!(a.transport, Transport::ReportFile);
        let report = PathBuf::from(r"C:/tmp/roslyn.sarif");
        assert_eq!(
            a.resolve_argv(&root(), Some(&report), true),
            vec![
                "build",
                "/p:ErrorLog=C:/tmp/roslyn.sarif,version=2.1",
                "-nologo"
            ]
        );
        // No report path ⇒ the `{report}` placeholder renders empty.
        assert_eq!(
            a.resolve_argv(&root(), None, true),
            vec!["build", "/p:ErrorLog=,version=2.1", "-nologo"]
        );
    }

    #[test]
    fn cargo_machete_has_no_fixed_args_and_reads_text() {
        let a = adapter(AuditToolId::CargoMachete);
        assert_eq!(a.parser, AuditParser::MacheteText);
        assert!(a.resolve_argv(&root(), None, true).is_empty());
        // Extra args still append after the (empty) fixed argv.
        assert_eq!(
            a.full_argv(&root(), None, true, &["--with-metadata".into()], ""),
            vec!["--with-metadata"]
        );
    }

    #[test]
    fn semgrep_quality_uses_best_practices_config() {
        let a = adapter(AuditToolId::SemgrepQuality);
        assert_eq!(
            a.resolve_argv(&root(), None, true),
            vec![
                "scan",
                "--config",
                "p/r2c-best-practices",
                "--sarif",
                "--quiet",
                "/proj/root"
            ]
        );
        assert_eq!(a.env, &[("PYTHONUTF8", "1")]);
        assert_eq!(a.category, Category::Quality);
    }

    #[test]
    fn ruleset_setting_overrides_the_config_slug() {
        // A non-empty per-tool `ruleset` replaces the built-in slug; empty
        // keeps it. Extra args still append after.
        let a = adapter(AuditToolId::SemgrepQuality);
        assert_eq!(
            a.full_argv(&root(), None, true, &[], "p/default"),
            vec![
                "scan",
                "--config",
                "p/default",
                "--sarif",
                "--quiet",
                "/proj/root"
            ]
        );
        assert_eq!(
            a.full_argv(&root(), None, true, &[], "")[2],
            "p/r2c-best-practices"
        );
    }

    #[test]
    fn default_ruleset_present_only_on_the_semgrep_tools() {
        assert_eq!(adapter(AuditToolId::Semgrep).default_ruleset(), Some("auto"));
        assert_eq!(
            adapter(AuditToolId::SemgrepQuality).default_ruleset(),
            Some("p/r2c-best-practices")
        );
        // A tool without the token ignores the setting entirely.
        assert_eq!(adapter(AuditToolId::Oxlint).default_ruleset(), None);
        assert_eq!(
            adapter(AuditToolId::Oxlint).full_argv(&root(), None, true, &[], "p/default"),
            vec!["--format", "sarif"]
        );
    }

    #[test]
    fn quality_exit_classification_edge_cases() {
        // cppcheck: exit 0 even with findings (no --error-exitcode), so 0 is
        // Clean (the report carries findings) and *any* non-zero is a tool
        // error — nothing is a "findings exit code".
        let cc = adapter(AuditToolId::Cppcheck);
        assert_eq!(cc.classify_exit(Some(0)), ExitClass::Clean);
        assert_eq!(cc.classify_exit(Some(1)), ExitClass::Error);
        assert_eq!(cc.classify_exit(Some(2)), ExitClass::Error);

        // typos: exit 2 = findings, exit 1 = error.
        let typos = adapter(AuditToolId::Typos);
        assert_eq!(typos.classify_exit(Some(0)), ExitClass::Clean);
        assert_eq!(typos.classify_exit(Some(2)), ExitClass::Findings);
        assert_eq!(typos.classify_exit(Some(1)), ExitClass::Error);

        // PMD: exit 4 = findings, 5 (recoverable error) = error.
        let pmd = adapter(AuditToolId::Pmd);
        assert_eq!(pmd.classify_exit(Some(4)), ExitClass::Findings);
        assert_eq!(pmd.classify_exit(Some(5)), ExitClass::Error);

        // The exit-1 tools.
        for id in [
            AuditToolId::Oxlint,
            AuditToolId::GolangciLint,
            AuditToolId::Ruff,
            AuditToolId::Eslint,
            AuditToolId::Knip,
            AuditToolId::CargoMachete,
            AuditToolId::DotnetAnalyzers,
            AuditToolId::SemgrepQuality,
        ] {
            let a = adapter(id);
            assert_eq!(a.classify_exit(Some(1)), ExitClass::Findings, "{id:?}");
            assert_eq!(a.classify_exit(Some(0)), ExitClass::Clean, "{id:?}");
        }
    }

    #[test]
    fn quality_applicability_gates() {
        // Language-gated tools are hidden on a project without their files.
        let empty = Census::default();
        for id in [
            AuditToolId::Oxlint,
            AuditToolId::GolangciLint,
            AuditToolId::Ruff,
            AuditToolId::Cppcheck,
            AuditToolId::Eslint,
            AuditToolId::Pmd,
            AuditToolId::DotnetAnalyzers,
            AuditToolId::Knip,
            AuditToolId::CargoMachete,
        ] {
            assert!(
                !adapter(id).applicable(&empty),
                "{id:?} gated off on empty census"
            );
            assert_eq!(adapter(id).category, Category::Quality, "{id:?}");
        }
        // typos + semgrep-quality are always applicable.
        assert!(adapter(AuditToolId::Typos).applicable(&empty));
        assert!(adapter(AuditToolId::SemgrepQuality).applicable(&empty));

        // Positive gates.
        assert!(adapter(AuditToolId::Oxlint).applicable(&Census::from_parts(&["tsx"], &[])));
        assert!(adapter(AuditToolId::Ruff).applicable(&Census::from_parts(&["py"], &[])));
        assert!(adapter(AuditToolId::Cppcheck).applicable(&Census::from_parts(&["hpp"], &[])));
        assert!(adapter(AuditToolId::Pmd).applicable(&Census::from_parts(&["java"], &[])));
        // golangci-lint via either the marker or the extension.
        assert!(
            adapter(AuditToolId::GolangciLint).applicable(&Census::from_parts(&[], &["go.mod"]))
        );
        assert!(adapter(AuditToolId::GolangciLint).applicable(&Census::from_parts(&["go"], &[])));
        // eslint via either config marker.
        assert!(
            adapter(AuditToolId::Eslint).applicable(&Census::from_parts(&[], &["eslint.config"]))
        );
        assert!(adapter(AuditToolId::Eslint).applicable(&Census::from_parts(&[], &[".eslintrc"])));
        // marker-only tools.
        assert!(adapter(AuditToolId::Knip).applicable(&Census::from_parts(&[], &["package.json"])));
        assert!(adapter(AuditToolId::CargoMachete)
            .applicable(&Census::from_parts(&[], &["Cargo.toml"])));
        assert!(adapter(AuditToolId::DotnetAnalyzers)
            .applicable(&Census::from_parts(&[], &["*.csproj"])));
    }
}

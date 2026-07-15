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

use crate::settings::AuditToolId;

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
}

impl Adapter {
    /// Render the fixed argv (no `extra_args`) for a concrete scan: choose the
    /// git vs `dir` template, then substitute [`Arg::Root`] / [`Arg::Report`].
    pub fn resolve_argv(&self, root: &Path, report: Option<&Path>, git_repo: bool) -> Vec<String> {
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
            })
            .collect()
    }

    /// The full argv the runner spawns: the fixed template followed by the
    /// user's per-tool `extra_args` (appended verbatim, after the fixed argv —
    /// the settings contract).
    pub fn full_argv(
        &self,
        root: &Path,
        report: Option<&Path>,
        git_repo: bool,
        extra_args: &[String],
    ) -> Vec<String> {
        let mut argv = self.resolve_argv(root, report, git_repo);
        argv.extend(extra_args.iter().cloned());
        argv
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
};

/// `semgrep scan --config auto --sarif --quiet <root>` → SARIF on stdout, with
/// `PYTHONUTF8=1` forced (semgrep's beta Windows support mangles output
/// otherwise).
static SEMGREP: Adapter = Adapter {
    argv: &[
        Arg::Lit("scan"),
        Arg::Lit("--config"),
        Arg::Lit("auto"),
        Arg::Lit("--sarif"),
        Arg::Lit("--quiet"),
        Arg::Root,
    ],
    dir_argv: None,
    transport: Transport::Stdout,
    env: &[("PYTHONUTF8", "1")],
    findings_exit_codes: &[1],
};

/// The built-in adapter for `id`. Total over the closed [`AuditToolId`] enum.
pub fn adapter(id: AuditToolId) -> &'static Adapter {
    match id {
        AuditToolId::OsvScanner => &OSV_SCANNER,
        AuditToolId::Gitleaks => &GITLEAKS,
        AuditToolId::Semgrep => &SEMGREP,
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
        let argv = a.full_argv(&root(), None, true, &["--offline".to_string()]);
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
        let argv = a.full_argv(&root(), Some(&report), true, &[]);
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
        let argv = a.full_argv(&root(), None, false, &["--config".into(), "p/ci".into()]);
        assert_eq!(
            argv,
            vec![
                "scan", "--config", "auto", "--sarif", "--quiet", "/proj/root", "--config", "p/ci",
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
    fn exit_code_classification_per_tool() {
        for id in [AuditToolId::OsvScanner, AuditToolId::Gitleaks, AuditToolId::Semgrep] {
            let a = adapter(id);
            assert_eq!(a.classify_exit(Some(0)), ExitClass::Clean, "{id:?} 0");
            assert_eq!(a.classify_exit(Some(1)), ExitClass::Findings, "{id:?} 1");
            assert_eq!(a.classify_exit(Some(2)), ExitClass::Error, "{id:?} 2");
            assert_eq!(a.classify_exit(Some(127)), ExitClass::Error, "{id:?} 127");
            // A killed child (no exit code) is a tool error, never findings.
            assert_eq!(a.classify_exit(None), ExitClass::Error, "{id:?} none");
        }
    }
}

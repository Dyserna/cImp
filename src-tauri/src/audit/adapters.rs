//! The audit fan-out's two shared vocabularies: **which umbrella a tool runs
//! under**, and **what a completed child's exit code means**.
//!
//! # What used to be here
//!
//! V23 built this file as a table of `static Adapter` rows — one per built-in
//! scanner — selected by a closed `AuditToolId` enum: argv template, transport,
//! forced env, exit-code semantics, applicability gate, parser. V38 Phase E
//! moved every one of those rows into `plugins/builtin/cimp-audit.json`, read
//! through the same manifest validator a dropped-in plugin goes through, so the
//! table and its enum are gone and the fourteen tools are data rather than code.
//!
//! What could NOT move is what is left. [`Category`] is the umbrella a scan
//! runs (a wire string the frontend and the `audit_start_scan` command both
//! carry), [`Transport`] is where a tool delivers its report, and
//! [`classify_exit`] is the rule that makes an audit tool's exit code mean
//! something different from a `run_check`'s. None of the three is per-tool
//! configuration; all three are contracts the runner applies to every tool of
//! either provenance.
//!
//! **Exit-code semantics live here, not in `checks`.** V22's `run_check` treats
//! a non-zero exit as a checker *failure*; audit tools invert that — `0` is
//! clean, a declared `findings_exit_codes` value means "ran fine, here are
//! findings" (a SUCCESS), and anything else is a genuine tool error.
//! [`classify_exit`] owns that distinction, and it is why this module still
//! exists rather than being folded into the runner: the sentence above is the
//! thing a reader needs, and it belongs next to the code it describes.

/// Which tab / section a tool belongs to. V25 keeps Security (the V23 trio) and
/// Quality (the V25 linters) in separate tabs with independent runs; the runner
/// filters a scan by this so a Quality scan never launches a Security tool.
///
/// Serializes lowercase (`"security"` / `"quality"`) — the wire string the
/// `audit_start_scan` command accepts and the `category` per-tool snapshot field
/// carries (mirrored in `src/lib/codeAudit/types.ts` as `AuditCategory`).
///
/// Since V38 a tool's umbrella is derived from its manifest KIND
/// (`security` → Security, `audit` → Quality), never from the category a plugin
/// files it under — decision 2's kind ⊥ category, and the property the security
/// floor rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Security,
    Quality,
}

/// Where a tool writes its report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// The report is emitted on the child's stdout (osv-scanner, semgrep).
    Stdout,
    /// The report is written to a temp file whose path is substituted for the
    /// `{report}` token in the argv; the child's stdout carries logs
    /// (gitleaks). The runner reads the file after the child exits.
    ReportFile,
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

/// Classify a completed child's exit code against the codes that mean
/// "findings, not failure" (`None` = killed / no code).
///
/// One rule for both provenances: the codes arrive as a `Vec<i32>` from a
/// manifest either way, and a second copy of this three-line match is a second
/// place for "exit 2 means typos found" to be forgotten in.
pub fn classify_exit(code: Option<i32>, findings_exit_codes: &[i32]) -> ExitClass {
    match code {
        Some(0) => ExitClass::Clean,
        Some(c) if findings_exit_codes.contains(&c) => ExitClass::Findings,
        _ => ExitClass::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inversion that makes an audit tool different from a `run_check`,
    /// exercised on the three shapes the built-in roster actually has: an
    /// exit-1 findings tool, `typos` (exit **2**), and `cppcheck` (no findings
    /// code at all — it exits 0 whether or not it found anything).
    #[test]
    fn exit_codes_classify_by_the_tools_declared_findings_set() {
        // The common shape.
        assert_eq!(classify_exit(Some(0), &[1]), ExitClass::Clean);
        assert_eq!(classify_exit(Some(1), &[1]), ExitClass::Findings);
        assert_eq!(classify_exit(Some(2), &[1]), ExitClass::Error);
        assert_eq!(classify_exit(Some(127), &[1]), ExitClass::Error);

        // `typos`: 2 is findings, and 1 is therefore an ERROR — the pair that
        // makes this a per-tool declaration rather than a constant.
        assert_eq!(classify_exit(Some(2), &[2]), ExitClass::Findings);
        assert_eq!(classify_exit(Some(1), &[2]), ExitClass::Error);

        // `cppcheck`: no findings code. Exit 0 is clean (its report carries the
        // findings), and every non-zero exit is a real failure.
        assert_eq!(classify_exit(Some(0), &[]), ExitClass::Clean);
        assert_eq!(classify_exit(Some(1), &[]), ExitClass::Error);

        // A killed child (no exit code) is a tool error, never findings — a
        // timeout must not be able to report a clean bill of health.
        assert_eq!(classify_exit(None, &[1]), ExitClass::Error);
        assert_eq!(classify_exit(None, &[]), ExitClass::Error);
    }

    /// `Category` is a wire string; the frontend's `AuditCategory` union and
    /// the `audit_start_scan` argument are both spelled from it.
    #[test]
    fn category_wire_names_are_lowercase() {
        assert_eq!(
            serde_json::to_value(Category::Security).unwrap(),
            serde_json::json!("security")
        );
        assert_eq!(
            serde_json::to_value(Category::Quality).unwrap(),
            serde_json::json!("quality")
        );
    }
}

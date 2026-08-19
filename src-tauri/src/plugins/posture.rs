//! V38 — the **sandbox posture** a plugin tool declares, applied identically at
//! every seam that spawns one.
//!
//! Phase C wrote these rules once, inside the audit runner, because the audit
//! fan-out was the only population that had them. Phase D adds two more seams
//! (`run_check`'s plugin checks and `run_command`'s registered commands), and
//! three copies of "what `required` means" is three chances for one of them to
//! mean something else. So the rules live here, above `sandbox` (which knows
//! nothing about manifests) and below the three seams (which know nothing about
//! each other).
//!
//! What a posture decides, in the order a spawn asks it:
//!
//! 1. **`extra_grants`** — screened by V33's refusal rules into [`GrantRow`]s,
//!    read+execute only ([`screen_extra_grants`]).
//! 2. **`unsupported`** — the boundary is not attempted at all, and the run says
//!    so once ([`unsupported_cfg`]).
//! 3. **`required`** — when the boundary could not be provided, the tool is NOT
//!    run and the refusal is the answer ([`required_refusal`]).
//! 4. **`runtime`** — which profile's grants apply, plus the declaration ⇄
//!    inference cross-check ([`runtime_canary`]).
//!
//! Deliberately NOT here: the *spawn* itself. Each seam spawns differently (a
//! resolved scanner binary, a shell command line, a model-named program), and a
//! shared spawn helper would have to know all three.

use std::path::Path;

use super::manifest::SandboxReq;
use crate::sandbox::{
    self, GrantAccess, GrantRow, GrantSource, RuntimeSelect, SandboxCfg, SkipReason,
};

/// The reason string every manifest-sourced [`GrantRow`] carries.
///
/// One constant rather than one literal per seam: it is user-visible text that
/// explains where the widening came from, and a second wording would make the
/// same grant read as two different things depending on which pipeline ran it.
const MANIFEST_GRANT_REASON: &str =
    "requested by a tool plugin's manifest (`extra_grants`) and granted by enabling that tool — \
     shown as a permission where it is enabled";

/// Screen a manifest's `extra_grants` into the rows a spawn may actually use.
///
/// `boundary_expected` says whether this spawn is going to prepare a boundary at
/// all (the sandbox is on AND the tool did not declare `unsupported`). It does
/// **not** change the screening — a refused path never becomes a [`GrantRow`],
/// whatever the switch says — only whether a refusal is REPORTED. A row that
/// says "this path was not granted, every other grant was applied" is a true
/// sentence inside `prepare` and a false one when nothing is being granted at
/// all: with the sandbox off the child can read that directory freely, and
/// telling the user it was withheld is worse than saying nothing (Phase C
/// review, B-C1). The honest row for that run is the unsandboxed/skip one the
/// seam already mints.
///
/// Refused paths are DROPPED and the rest still apply: a bad grant must not
/// brick a tool, and it must not silently widen the boundary either.
pub fn screen_extra_grants(
    seam: &str,
    root: &Path,
    grants: &[String],
    boundary_expected: bool,
) -> Vec<GrantRow> {
    let mut rows = Vec::new();
    for grant in grants {
        let path = std::path::PathBuf::from(grant);
        match sandbox::extra_grant_refusal_live(&path) {
            Some(why) => {
                if boundary_expected {
                    sandbox::record_grant_refused(seam, root, &path, why, GrantSource::Manifest);
                }
            }
            None => rows.push(GrantRow {
                path,
                // READ+EXECUTE, never full: `extra_grants` exists for a tool
                // that must READ something no profile covers (a rules tree, a
                // runtime image). The two places a tool legitimately WRITES are
                // already granted — the project root and, for a report-file
                // tool, cImp's own report directory — so a write ACE here would
                // only ever widen the boundary past what the field is for.
                access: GrantAccess::ReadExecute,
                is_file: false,
                reason: MANIFEST_GRANT_REASON,
                // Absent is not fatal: a manifest is written once for many
                // machines, and refusing to sandbox a tool because an optional
                // rules directory is missing punishes a fine machine.
                required: false,
            }),
        }
    }
    rows
}

/// The config a spawn should PLAN with, given what the manifest declared.
///
/// `Some(disabled)` for `sandbox: unsupported` — and the caller must plan with
/// that, not with the real config. Passing the real one and discarding the plan
/// would make the same run *plus* durable changes to the user's machine (ACEs,
/// a mapped drive) on behalf of a tool that declared it cannot use either.
/// `None` means "nothing to override; plan with what you have".
///
/// Mints the visible row as a side effect, once per (seam, subject) per session:
/// running outside the boundary is an informed choice, and an informed choice
/// nobody can see is just a silent one.
pub fn unsupported_cfg(
    seam: &str,
    root: &Path,
    subject: &str,
    req: SandboxReq,
) -> Option<SandboxCfg> {
    if req != SandboxReq::Unsupported {
        return None;
    }
    sandbox::record_declared_unsandboxed(seam, root, subject);
    Some(SandboxCfg::disabled())
}

/// The refusal message for a `sandbox: required` tool whose boundary could not
/// be provided, or `None` when this spawn may proceed.
///
/// `required` means never run unprotected — **including when the master switch
/// is off**, which is the case an author cannot see and a user can. A manifest
/// that says "this tool must be confined" is not overridden by a global
/// preference; the tool is simply missing from this run, loudly, in both the
/// lane and whatever surface the seam reports through.
pub fn required_refusal(
    seam: &str,
    root: &Path,
    subject: &str,
    req: SandboxReq,
    reason: &SkipReason,
) -> Option<String> {
    if req != SandboxReq::Required {
        return None;
    }
    let why = match reason {
        SkipReason::OffUser => "OS sandboxing is switched off in cImp settings".to_string(),
        SkipReason::Unavailable(r) => r.clone(),
    };
    sandbox::record_sandbox_required_refusal(seam, root, subject, &why);
    Some(format!(
        "not run: this tool's manifest declares `sandbox: required` and the OS sandbox could \
         not be provided — {why} (see the sandbox lane)"
    ))
}

/// The declaration ⇄ inference cross-check (design authority: "Declaration and
/// inference cross-check").
///
/// cImp runs with what the manifest DECLARED — inference cannot know a runtime
/// it has never met — and records the disagreement rather than silently trusting
/// either side. A no-op unless a profile was declared: `auto` IS inference, and
/// `none` is a statement inference has nothing to say about.
pub fn runtime_canary(
    seam: &str,
    root: &Path,
    subject: &str,
    select: &RuntimeSelect,
    resolved: &Path,
) {
    let RuntimeSelect::Profile(declared) = select else {
        return;
    };
    let lookup = |k: &str| std::env::var_os(k);
    let is_dir = |d: &Path| d.is_dir();
    let machine = sandbox::Machine {
        env: &lookup,
        is_dir: &is_dir,
    };
    let inferred = sandbox::inferred_runtime_ids(resolved, &machine);
    if !inferred.is_empty() && !inferred.contains(declared) {
        sandbox::record_runtime_mismatch(seam, root, subject, declared, &inferred);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused grant never becomes a row, whether or not a boundary is being
    /// prepared; the flag only decides whether the refusal is REPORTED.
    #[test]
    fn a_refused_grant_is_dropped_from_the_rows() {
        let root = std::env::temp_dir();
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from);
        let Some(home) = home else {
            return; // no home on this machine: nothing to build a refused path from
        };
        let ssh = home.join(".ssh");
        let ok = root.join("cimp-posture-ok");
        for expected in [true, false] {
            let rows = screen_extra_grants(
                "test-seam",
                &root,
                &[
                    ssh.to_string_lossy().into_owned(),
                    ok.to_string_lossy().into_owned(),
                ],
                expected,
            );
            assert_eq!(rows.len(), 1, "the credential store must never be granted");
            assert_eq!(rows[0].path, ok);
            assert_eq!(rows[0].access, GrantAccess::ReadExecute);
        }
    }

    /// `required` refuses even when the sandbox is off by the user's own
    /// choice; `optional`/`unsupported` never refuse.
    #[test]
    fn required_refuses_and_the_other_two_do_not() {
        let root = std::env::temp_dir();
        let off = SkipReason::OffUser;
        let msg = required_refusal("test-seam", &root, "acme", SandboxReq::Required, &off)
            .expect("required must refuse");
        assert!(msg.contains("switched off"), "{msg}");
        assert!(msg.contains("sandbox: required"), "{msg}");
        for req in [SandboxReq::Optional, SandboxReq::Unsupported] {
            assert!(required_refusal("test-seam", &root, "acme", req, &off).is_none());
        }
    }

    /// `unsupported` plans with a DISABLED config; the other two plan with
    /// whatever the caller has.
    #[test]
    fn unsupported_plans_with_a_disabled_config() {
        let root = std::env::temp_dir();
        let cfg = unsupported_cfg("test-seam", &root, "acme", SandboxReq::Unsupported)
            .expect("unsupported must override");
        assert!(!cfg.enabled);
        for req in [SandboxReq::Optional, SandboxReq::Required] {
            assert!(unsupported_cfg("test-seam", &root, "acme", req).is_none());
        }
    }
}

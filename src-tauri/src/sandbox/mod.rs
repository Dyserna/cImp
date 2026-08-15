//! V33 Phase A — the OS sandbox layer for **agent-initiated child processes**.
//!
//! Engine: Windows AppContainer, chosen by spike S1
//! (`docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`, user decision
//! 2026-08-15 closing milestone decision 2). Linux gets Landlock in Phase D;
//! until then non-Windows reports `Unavailable` and children run exactly as
//! before — loudly, per decision 5, never silently.
//!
//! **Scope.** This layer wraps the [`SpawnClass::AgentSpawn`] seam that a model
//! drives most directly: `run_command` children (`offload/tools/run_command.rs`).
//! Tab spawns (ConPTY) are Phase B behind spike S3; `run_check`/audit children
//! run through a shell today and follow once the engine has soaked. Host spawns
//! (`spawn_ledger::SpawnClass::HostSpawn`) are **never** sandboxed — see the
//! ledger's reasons column.
//!
//! **What the boundary is (and honestly is not).** A sandboxed child can
//! read+write the project root, read the OS dirs and each granted tool's own
//! code/config/caches — and nothing else. Writes outside the root are denied,
//! reads of credential dirs, other projects and cImp's own secrets are denied.
//! It is *not* "reads nothing outside the root": executing `git` means reading
//! `git.exe`. Every widening beyond the root is a reviewed grant with a reason
//! (decision 3), stamped once per machine against the **stable** container SID.
//!
//! **The off switch reaches this layer only** (decision 17): `sandbox.enabled =
//! false` removes the AppContainer wrapper and nothing more. Job objects, the
//! minimal environment and the injection-layer fixes are lifecycle/containment
//! correctness and remain unconditional. The two negative states are distinct
//! and stay distinct (C10): [`SkipReason::OffUser`] is a choice,
//! [`SkipReason::Unavailable`] is a missing prerequisite; collapsing them is
//! how a broken prerequisite hides behind a deliberate setting.
//!
//! **Degradation is loud, never silent** (decision 5): every skip reason is
//! recorded as a `sandbox` activity row (its own retention lane, per #51's
//! pick-a-lane-on-purpose rule) the first time it occurs in a session, and the
//! child then runs unsandboxed — this is a hardening layer over a working
//! product, not a gate that bricks tool calls on a missing prerequisite.

#[cfg(windows)]
pub mod windows;

use std::path::{Path, PathBuf};

/// The runtime slice of `Settings::sandbox` a spawn seam needs. Carried on
/// `ToolCtx` beside the command allowlist rather than read from a global, so
/// the headless `--offload-mcp` child and unit tests get exactly the config
/// their constructor passed — the same plumbing discipline as
/// `command_allowlist` itself.
#[derive(Debug, Clone, Default)]
pub struct SandboxCfg {
    /// Master switch (decision 17; OS layer only).
    pub enabled: bool,
    /// Grant the `internetClient` capability to sandboxed children. Default
    /// off: a read-only probe needs no egress, and S1 measured that on a
    /// Public-profile NIC this capability opens the LAN too — per-host
    /// scoping is S4/WFP work, so the honest granularity today is all/none.
    pub allow_network: bool,
    /// Extra directories granted read+execute inside the sandbox — the
    /// user-curated rows of the decision-3 grant table (tools in
    /// non-standard locations that grant-on-first-use can't infer).
    pub extra_grant_dirs: Vec<PathBuf>,
}

impl SandboxCfg {
    /// The context-provides-none default: sandbox off, labeled as such.
    /// `ToolCtx::new` uses this so a new construction site gets the safe,
    /// honest shape; real paths opt in with the settings value.
    pub fn disabled() -> Self {
        Self::default()
    }
}

/// Why a spawn is NOT running inside the sandbox. The two variants are the
/// C10 states and must never merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `sandbox.enabled = false` — the user's choice.
    OffUser,
    /// A prerequisite is missing or still becoming ready; the reason string
    /// is user-facing (Events row) and starts with what failed.
    Unavailable(String),
}

impl SkipReason {
    /// The Events-row label. Kept short and stable; the detail rides the
    /// record's response payload.
    pub fn label(&self) -> &'static str {
        match self {
            SkipReason::OffUser => "off (user choice)",
            SkipReason::Unavailable(_) => "unavailable",
        }
    }
}

/// The decision for one spawn: run it inside the container, or run it plain
/// and say why.
pub enum Plan {
    #[cfg(windows)]
    Sandboxed(windows::Prepared),
    Plain(SkipReason),
}

/// Decide how to run one `run_command` child, doing all sandbox preparation
/// (profile, grants, drive mapping) that the decision needs.
///
/// `env` is the exact minimal-environment pair list the plain spawn would
/// use — the sandbox path adds its redirections on top (TEMP and tool caches
/// into the root) rather than composing a second environment.
pub async fn plan(
    cfg: &SandboxCfg,
    program: &Path,
    root: &Path,
    env: &[(&str, std::ffi::OsString)],
) -> Plan {
    if !cfg.enabled {
        return Plan::Plain(SkipReason::OffUser);
    }
    #[cfg(windows)]
    {
        match windows::prepare(cfg, program, root, env).await {
            Ok(prepared) => Plan::Sandboxed(prepared),
            Err(reason) => Plan::Plain(SkipReason::Unavailable(reason)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (program, root, env);
        Plan::Plain(SkipReason::Unavailable(
            "no OS sandbox engine on this platform yet (Linux Landlock is V33 Phase D)".into(),
        ))
    }
}

/// Record one skip loudly, once per distinct reason per session — repeat
/// occurrences are the same fact, and a row per spawn would just let this
/// lane crowd itself out of its retention window.
pub fn record_skip(reason: &SkipReason, program: &Path, root: &Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let key = match reason {
        SkipReason::OffUser => "off".to_string(),
        SkipReason::Unavailable(r) => r.clone(),
    };
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(key) {
            return;
        }
    }
    // `off (user choice)` is still recorded — once — so the Events feed
    // answers "was this run sandboxed?" without the user having to remember
    // what the switch was set to at the time (C10's two-states rule).
    let detail = match reason {
        SkipReason::OffUser => String::new(),
        SkipReason::Unavailable(r) => r.clone(),
    };
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Sandbox,
            crate::activity::now_ms(),
            root.to_string_lossy().into_owned(),
            "run_command".into(),
            "unsandboxed".into(),
            format!(
                "{} — {}",
                reason.label(),
                program.file_name().unwrap_or_default().to_string_lossy()
            ),
            0,
            0,
            // `ok` mirrors whether this state is a chosen one: a user choice is
            // not a failure; a missing prerequisite is.
            matches!(reason, SkipReason::OffUser),
            crate::activity::Attribution::Headless,
            None,
        ),
        request: String::new(),
        response: detail,
    });
}

/// Record a sandbox-side lifecycle fact — today, the one-time ACL grants that
/// prepare a machine (`tool = "grant"`) — into the same lane.
///
/// `#[allow(dead_code)]` off Windows: the only caller is the AppContainer
/// engine, and Landlock (Phase D) will be the second.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn record_event(root: &Path, tool: &str, target: String, detail: String, ok: bool) {
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Sandbox,
            crate::activity::now_ms(),
            root.to_string_lossy().into_owned(),
            "run_command".into(),
            tool.to_string(),
            target,
            0,
            0,
            ok,
            crate::activity::Attribution::Headless,
            None,
        ),
        request: String::new(),
        response: detail,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cfg_yields_off_user() {
        let cfg = SandboxCfg::disabled();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let plan = rt.block_on(plan(
            &cfg,
            Path::new("C:/x/y.exe"),
            Path::new("C:/proj"),
            &[],
        ));
        match plan {
            Plan::Plain(SkipReason::OffUser) => {}
            _ => panic!("disabled cfg must plan a plain spawn with OffUser"),
        }
    }

    #[test]
    fn skip_labels_stay_distinct() {
        // C10: the two negative states must never collapse into one string.
        assert_ne!(
            SkipReason::OffUser.label(),
            SkipReason::Unavailable("x".into()).label()
        );
    }

    /// Decision 17, as a test rather than a comment: the master switch governs
    /// the OS layer ONLY. `SandboxCfg` is the whole of what the switch reaches
    /// at the spawn seam, so anything unconditional must be absent from it —
    /// if a future change moves the minimal environment or the job object
    /// behind this struct, this test is what notices.
    #[test]
    fn the_off_switch_reaches_the_os_layer_only() {
        let cfg = SandboxCfg::disabled();
        // The struct carries only OS-boundary knobs. Job objects
        // (`process_guard`), the C2 minimal environment (`run_command::
        // CHILD_ENV`) and the injection-layer fixes are deliberately not
        // representable here — they stay on regardless of `enabled`.
        assert!(!cfg.enabled);
        assert!(!cfg.allow_network);
        assert!(cfg.extra_grant_dirs.is_empty());
        // Guard the shape itself: a three-field struct is the contract. Adding
        // a field is fine; adding one that switches OFF a non-OS protection is
        // the thing decision 17 forbids, and the reviewer of that diff sees
        // this assertion's comment.
        let SandboxCfg {
            enabled: _,
            allow_network: _,
            extra_grant_dirs: _,
        } = cfg;
    }

    /// A settings-off sandbox must be reported as a *choice*, and an engine
    /// failure as *unavailable* — never the reverse, because collapsing them
    /// is how a broken prerequisite hides behind a deliberate setting (C10).
    #[test]
    fn off_is_a_choice_not_an_unavailability() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let plan = rt.block_on(plan(
            &SandboxCfg::disabled(),
            Path::new("C:/x/git.exe"),
            Path::new("C:/proj"),
            &[],
        ));
        match plan {
            Plan::Plain(SkipReason::OffUser) => {}
            Plan::Plain(SkipReason::Unavailable(r)) => {
                panic!("a disabled switch was reported as unavailable: {r}")
            }
            #[cfg(windows)]
            Plan::Sandboxed(_) => panic!("a disabled switch still sandboxed the spawn"),
        }
    }

    /// On a platform with no engine, the reason must NAME the missing thing —
    /// decision 5's "loud, never silent" applied to the string a user reads in
    /// the Events row.
    #[cfg(not(windows))]
    #[test]
    fn non_windows_says_what_is_missing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut cfg = SandboxCfg::disabled();
        cfg.enabled = true;
        let plan = rt.block_on(plan(&cfg, Path::new("/usr/bin/git"), Path::new("/proj"), &[]));
        match plan {
            Plan::Plain(SkipReason::Unavailable(r)) => {
                assert!(r.contains("Landlock"), "reason must name the gap: {r}");
            }
            _ => panic!("an enabled sandbox on a platform with no engine must be Unavailable"),
        }
    }
}

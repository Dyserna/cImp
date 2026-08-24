//! Workbench (checkpoints, diff, worktrees) and the sandbox knobs.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

/// V13 §0.4: the Workbench feature's settings. `enabled` is the master
/// switch for the tab itself (default **on** — the tab is cheap and each
/// section gates its own behavior); `checkpoints` is the shadow-repo
/// snapshot feature (default **off** in V1 — proposed on-by-default once the
/// shadow-repo cost is validated on a large real repo, per the milestone's
/// open decision 2). The five `checkpoint_*` fields tune retention (`_max`,
/// `_max_age_days`) and the debounced burst trigger (`_burst_files`,
/// `_burst_window_s`, `_min_gap_s`) that Phase C's fallback-to-activity
/// snapshot trigger reads.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct WorkbenchSettings {
    /// Master switch: the reserved Workbench tab exists. Off = no tab, no
    /// fs-batch event/broadcast, no checkpoint scheduling.
    pub enabled: bool,
    /// Automatic checkpoint snapshots (Phase C's shadow git repo). Off by
    /// default in V1 — the tab's Diff/Worktrees sections work without it;
    /// Timeline needs this on.
    pub checkpoints: bool,
    /// Ring-buffer cap: the shadow repo keeps at most this many checkpoints
    /// (oldest pruned first by `shadow::gc`, subject to `checkpoint_max_age_days`).
    pub checkpoint_max: u32,
    /// Age cap in days: checkpoints older than this are pruned regardless of
    /// how far under `checkpoint_max` the ring is.
    pub checkpoint_max_age_days: u32,
    /// Burst trigger: at least this many distinct changed paths within
    /// `checkpoint_burst_window_s` (and at least `checkpoint_min_gap_s` since
    /// the last snapshot) fires an "activity" checkpoint — the fallback that
    /// covers shell-tab edits and any flow that doesn't go through the
    /// prompt-tap trigger.
    pub checkpoint_burst_files: u32,
    /// Time window (seconds) the burst-file count above is measured over.
    pub checkpoint_burst_window_s: u32,
    /// Minimum seconds between two automatic snapshots FROM THE SAME SOURCE,
    /// so a rapid-fire prompt sequence or a noisy save loop can't spam the
    /// shadow repo with near-duplicate commits.
    ///
    /// "Source" is the AI tab the prompt came from — the burst trigger, which
    /// belongs to no tab, is its own source. The gap is therefore enforced per
    /// `(project, tab)` rather than per project (V33): with two AI tabs on one
    /// project, each tab's prompt can take its own checkpoint inside the
    /// other's cooldown, which is what lets the Timeline say which checkpoint
    /// was live for a GIVEN tab. The cost, accepted deliberately: snapshot
    /// volume scales with the number of active tabs on a project.
    pub checkpoint_min_gap_s: u32,
}

/// V33 Phase A — OS-level sandboxing of agent-initiated child processes.
///
/// **Locked decision 16: one top-level category holds every sandboxing
/// setting.** Not scattered into Tabs / Local task offload / Per-tab overrides.
/// Sibling to `Injection protection`, deliberately **not merged into it**: V32
/// constrains a compromised model at the tool layer, V33 makes the OS enforce a
/// boundary the model cannot negotiate with, and merging them would let a user
/// believe one delivers the other. Membership test for anything added here:
/// *does this control the boundary the OS enforces?* — not *did V33 add it?*
///
/// **Locked decision 17: [`enabled`](Self::enabled) reaches the OS layer
/// ONLY.** Off ⇒ no per-spawn AppContainer wrapper on Windows, no Landlock
/// ruleset on Linux (V33 Phase D), and — when it lands — no Max Paranoia. The
/// same three fields govern both engines: there is no Linux-only setting, and
/// `extra_grant_dirs` means the same thing on both (a reviewed read+execute
/// widening). Unconditional regardless of this switch, and
/// therefore absent from this struct: job-object kill-on-close (lifecycle
/// correctness — switching it off reintroduces orphans, a bug not a freedom),
/// `run_command`'s minimal environment (it withholds credentials, not
/// capability), and the V32/V33 injection-layer fixes.
///
/// The two negative states — `off (user choice)` and `unavailable (prerequisite
/// missing)` — are **distinct and never collapsed**; see
/// [`sandbox::SkipReason`](crate::sandbox::SkipReason). This struct only
/// carries the first; the second is discovered at spawn time.
///
/// ⚠ **A compromised model must not be able to flip this switch.** That rests
/// on it being a settings write with no tool-exposed path: `run_command` cannot
/// reach `settings.json` (it is outside every `allowed_root` and the sandbox
/// itself denies the write), and no MCP tool writes settings. Verified rather
/// than inherited — the V32 run found a comment standing in for a check six
/// times.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct SandboxSettings {
    /// Master switch for the OS sandbox layer (decision 17).
    ///
    /// Default **false**: Phase A ships the engine, and the grant ladder can
    /// still surprise a machine whose toolchains live in Administrators-owned
    /// directories. Opt-in first, default-on proposed once the live-verify
    /// items in `docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md` have soaked
    /// — the same posture `workbench.checkpoints` shipped with.
    pub enabled: bool,
    /// V33 Phase B — also sandbox the **AI-tool tabs** (Claude, claude-local,
    /// OpenCode), not just the tool seams.
    ///
    /// Effective only when [`enabled`](Self::enabled) is also true: this is a
    /// scope widener inside the OS layer, never a second master switch. The two
    /// off states stay distinguishable in the Events lane (the skip row's detail
    /// names which switch was off), because "I turned sandboxing off" and "I
    /// left tabs out of it" are different user intents.
    ///
    /// Default **false**, and a bigger step than [`enabled`](Self::enabled) is:
    /// a tab IS the agent, so confining it confines everything the agent
    /// afterwards runs — including a `git push` whose credential helper now
    /// cannot read the user's store. Opt in deliberately.
    ///
    /// **Plain Shell tabs are never sandboxed by this.** A shell tab is the
    /// user's own hands, not an agent seam; confining it would be cImp deciding
    /// what its user may do on their own machine.
    pub tabs: bool,
    /// Give sandboxed children the `internetClient` capability.
    ///
    /// Default **false** — a read-only probe needs no egress. Spike S1
    /// measured that on a Public-profile NIC this single capability opens the
    /// LAN as well as the internet (capabilities are class-granular), so the
    /// honest choice today is all-or-nothing; per-host scoping per locked
    /// decision 4 is WFP work (spike S4).
    ///
    /// **This knob governs `run_command` / `run_check` / the audit scanners
    /// only — NOT tabs.** A sandboxed AI tab always gets `internetClient`
    /// (locked decision B3): an AI CLI that cannot reach its own model endpoint
    /// is a bricked tab, not a hardened one. See
    /// [`crate::sandbox::tabs::tab_sandbox_cfg`].
    pub allow_network: bool,
    /// Extra directories granted read+execute inside the sandbox — the
    /// user-curated rows of decision 3's grant table.
    ///
    /// The spawn path already grants the resolved program's own install
    /// directory, which covers the common case; this is for a toolchain that
    /// reaches sideways (a compiler in one tree calling a linker in another).
    /// Empty by default.
    pub extra_grant_dirs: Vec<String>,
}

// `Default` is derived: every field's "off / empty" is exactly the milestone
// default (sandbox off until the grant ladder soaks, no network capability, no
// extra grants), so a hand-written impl would only be a place for the two to
// drift apart.

impl Default for WorkbenchSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            checkpoints: false,
            checkpoint_max: 100,
            checkpoint_max_age_days: 7,
            checkpoint_burst_files: 5,
            checkpoint_burst_window_s: 60,
            checkpoint_min_gap_s: 120,
        }
    }
}

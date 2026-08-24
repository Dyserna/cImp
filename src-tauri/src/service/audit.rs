//! The Code Audit use cases: the Detect probe behind the Tool Plugins pane, and
//! the census refresh that tells the panel which tools are applicable before the
//! first scan.
//!
//! ## What the A1 audit run found — and what it deliberately did not move
//!
//! Six commands, and only two of them have a body. The other four
//! (`audit_start_scan`, `audit_cancel_scan`, `audit_snapshot`,
//! `audit_effective_roster`) are one call each on the `Arc<AuditState>` Tauri
//! already injects, so they stay at the wire boundary with a note; wrapping them
//! would add a hop that says nothing the runner's own method does not.
//!
//! **The `--version` spawn stays in [`crate::audit`].** `audit::detect_tool` is
//! a row in [`crate::spawn_ledger`]'s `CORE_LEDGER` — `file: "audit/mod.rs",
//! symbol: "detect_tool", count: 1` — and the ledger's exhaustiveness tripwire
//! scans that file's `include_str!` text. Moving the `Command::new` here would
//! have meant moving the ledger row and re-pointing `ledger_sources` in the same
//! commit, for no gain: what this use case actually owns is the *registry join*
//! — resolving a tool key against this project's effective tools, and answering
//! honestly when the plugin that declared it is gone. That join is what moved.
//! The spawn is one call away, behind a `pub(crate)` fn, and the ledger row is
//! byte-identical.
//!
//! ## What did NOT change
//!
//! [`AuditService::detect`] is still **settings-read-only**: the probe reports,
//! and the found path becomes configuration only through the same UI act a
//! Browse… would have been. That is what lets it be called freely without a
//! click ever silently changing what a scan launches. The live `path` from the
//! Settings input is still passed explicitly so a just-typed value cannot race
//! the fire-and-forget `settings_update` push, and an unknown `tool_key` is
//! still a normal result (`found: false` with a "press Rescan" message) rather
//! than an error.

use std::path::Path;
use std::sync::Arc;

use crate::audit::{AuditDetectResult, AuditSnapshot, AuditState};
use crate::error::{AppError, AppResult};
use crate::settings::SettingsHandle;

/// The Code Audit use cases, over one borrowed handle — same shape and rationale
/// as [`crate::service::tabs::TabService`].
pub struct AuditService<'a> {
    settings: &'a SettingsHandle,
}

impl<'a> AuditService<'a> {
    pub fn new(settings: &'a SettingsHandle) -> Self {
        Self { settings }
    }

    /// Resolve one registered tool honoring its configured `path` and probe
    /// `<tool> --version`. Always returns `Ok`: a not-found tool is a normal
    /// result (`found = false`), not an error.
    ///
    /// `tool_key` is the registry key (`cimp-audit@1/gitleaks`, or a user
    /// plugin's `name@version/tool-id`), and `path` is the LIVE value from the
    /// Settings input — passed explicitly so a just-typed value cannot race the
    /// fire-and-forget `settings_update` push. `None` falls back to what the
    /// registry resolved for this project.
    ///
    /// # What it searches for, and why that is not the run-time rule
    ///
    /// With an empty `path` the probe searches `ebin` → `PATH` for the name
    /// [`crate::plugins::registry::probe_command_name`] derives — for EVERY
    /// tool, including a user plugin's. That is wider than
    /// `EffectiveTool::resolves_by_name`, which stays exactly as it was: run time
    /// still requires a stored path for a user plugin (decision 7/10), and
    /// nothing about a Detect click makes a path-less tool runnable.
    ///
    /// The two differ because they answer different questions. Run time asks
    /// "may cImp spawn a binary the user never pointed it at?" — no. Detect asks
    /// "is this tool on this machine?", pressed by the user, about one tool, with
    /// the answer shown to them. Gating the second on the first made Detect
    /// report "not found on PATH or ebin" for every starter-pack tool without
    /// ever looking, which was untrue as well as useless.
    pub async fn detect(
        &self,
        root: &Path,
        tool_key: &str,
        path: Option<String>,
    ) -> AppResult<AuditDetectResult> {
        let settings = self.settings.current();
        let tool = crate::plugins::registry::effective_tools(
            &crate::plugins::snapshot_or_scan(),
            &settings.tool_plugins,
            Some(root),
        )
        .into_iter()
        .find(|t| t.tool_key == tool_key);
        let Some(tool) = tool else {
            return Ok(unknown_tool(tool_key));
        };
        let override_path = path.or_else(|| tool.path.clone()).unwrap_or_default();
        Ok(crate::audit::detect_tool(
            &override_path,
            tool.probe_name().as_deref(),
            tool.manifest.project_local_bin.as_deref(),
            Some(root),
        )
        .await)
    }
}

/// The answer for a `tool_key` no longer in the registry. A normal result, not
/// an error: the plugin that declared it may simply have been removed, and the
/// message says what to do about it.
///
/// Split out so the shape is assertable without a plugin scan or a probe.
fn unknown_tool(tool_key: &str) -> AuditDetectResult {
    AuditDetectResult {
        found: false,
        path: None,
        version: None,
        error: Some(format!(
            "no registered tool `{tool_key}` — the plugin that declared it may have been \
             removed; press Rescan"
        )),
    }
}

/// Take (or reuse, ≤60 s cache) the project's language census outside a scan,
/// apply quality auto-selection when it's on, and return the full snapshot — so
/// tab mount and the Settings section know applicability (chip gating, "not
/// applicable" hints, auto-selected checkboxes) before the first scan.
///
/// The walk is bounded but can take a couple of seconds cold, hence the
/// blocking-task hop. No-op passthrough while the feature is disabled or a scan
/// is in flight.
///
/// Free rather than an [`AuditService`] method: it reads no settings of its own,
/// so a service would be a handle it never touches.
pub async fn refresh_census(audit: Arc<AuditState>) -> AppResult<AuditSnapshot> {
    tauri::async_runtime::spawn_blocking(move || audit.refresh_census())
        .await
        .map_err(|e| AppError::Audit(format!("census task failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Previously untested.** A Detect click on a tool whose plugin has been
    /// removed must come back as a normal not-found answer that names the tool
    /// and says what to do — not an error toast, and not a silent `found: false`
    /// with no reason, which reads as "this tool is not installed" when the
    /// truth is that cImp no longer knows what the tool is.
    #[test]
    fn a_tool_key_the_registry_lost_answers_with_a_reason_not_an_error() {
        let answer = unknown_tool("gone@1/scanner");
        assert!(!answer.found);
        assert!(answer.path.is_none());
        assert!(answer.version.is_none());
        let msg = answer.error.expect("a not-found answer always carries why");
        assert!(
            msg.contains("gone@1/scanner"),
            "the message names the key the caller asked about: {msg}"
        );
        assert!(
            msg.contains("Rescan"),
            "the message names the fix, because the tool is not what is missing: {msg}"
        );
    }
}

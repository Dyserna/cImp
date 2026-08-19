//! V38 Phase E — the **embedded built-in plugins**: the tool definitions cImp
//! ships, expressed in the same manifest schema a dropped-in file uses and read
//! through the same validator.
//!
//! # Why the fourteen scanners are a plugin
//!
//! Before this phase they were a `static` table of `Adapter` rows selected by a
//! closed `AuditToolId` enum, with their own settings struct, their own
//! scope-promotion machinery and their own parser namespace. That worked, and it
//! is exactly the "hardcoded tier" the milestone set out to delete — not because
//! two tiers is untidy, but because a framework whose own author needed an
//! escape hatch has not been shown to be sufficient. **Migrating them IS the
//! proof.** Everything the built-in tier could express is now something a
//! manifest can express, except the four fields marked built-in-only in
//! [`super::manifest`] — and each of those is a documented, tested relaxation
//! rather than a private door.
//!
//! # What "built in" means, precisely
//!
//! Not the name. [`Provenance`] is stamped **here**, by the code that reads a
//! `&'static str` compiled into the binary; a scanned file gets
//! [`Provenance::User`] no matter what it is called, and the `cimp-` prefix is
//! refused on that path. Every gate that gives the built-in tier something extra
//! — the grandfathered output gate, the `PATH` fallback, the legacy findings
//! parsers, the security floor — keys off that stamp.
//!
//! # Failure posture
//!
//! An embedded manifest that does not parse is a **build defect**, not a user
//! problem, so it is a test failure (`the_embedded_manifests_all_load`) rather
//! than a panic in front of somebody's audit. At run time the error is carried
//! in the [`PluginSet`] like any other, which means the settings pane and the
//! `plugin` Events lane say what happened instead of the roster quietly
//! shrinking.

use std::sync::{Arc, OnceLock};

use super::loader::{LoadedPlugin, PluginError, PluginErrorKind, PluginSet};
use super::manifest::{self, Provenance};

/// `(display path, JSON)` for every embedded plugin.
///
/// The display path is what the settings pane and an Events row show as the
/// file, and it deliberately does not look like a filesystem path a user could
/// go and edit: these definitions live in the binary, and pointing somebody at
/// a path that is not there would be worse than saying so.
const EMBEDDED: &[(&str, &str)] = &[(
    "<built in: cimp-audit>",
    include_str!("builtin/cimp-audit.json"),
)];

/// The embedded set, parsed once per process.
///
/// Memoized because it cannot change: the input is compiled in. A `Rescan`
/// re-walks the plugins directory and re-uses this — re-parsing a constant on
/// every scan would be work that can only produce the same answer.
pub fn plugin_set() -> Arc<PluginSet> {
    static SET: OnceLock<Arc<PluginSet>> = OnceLock::new();
    SET.get_or_init(|| Arc::new(parse_embedded())).clone()
}

fn parse_embedded() -> PluginSet {
    let mut set = PluginSet {
        dir: "<built in>".to_string(),
        scanned_at_ms: crate::activity::now_ms(),
        ..PluginSet::default()
    };
    for (path, text) in EMBEDDED {
        match manifest::parse(text, Provenance::Builtin) {
            Ok(m) => set.plugins.push(LoadedPlugin {
                path: (*path).to_string(),
                provenance: Provenance::Builtin,
                key: m.key(),
                manifest: m,
            }),
            // Unreachable in a shipped build (the test below is what keeps it
            // so), and still a value rather than a panic: a broken embedded
            // definition must degrade to "this plugin did not load, here is
            // why" like every other one, not take the app down mid-audit.
            Err(f) => set.errors.push(PluginError {
                kind: PluginErrorKind::Invalid,
                paths: vec![(*path).to_string()],
                key: f.key,
                reason: format!(
                    "cImp's own built-in tool definitions failed to load — this is a defect in \
                     this build, not in your configuration: {}",
                    f.error
                ),
            }),
        }
    }
    set
}

/// The plugin key the built-in audit tools live under.
///
/// Named once, because it is a **settings key**: the container stores enables,
/// timeouts, variable values and binary paths under `cimp-audit@1/<tool-id>`,
/// and the v32 → v33 migration writes exactly those strings. Bumping the
/// version in the manifest would orphan every one of them, which is why the
/// version is `1` and means "the identity of this shipped set", not "the cImp
/// release it came in".
pub const AUDIT_PLUGIN_KEY: &str = "cimp-audit@1";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::manifest::{IngestReq, SandboxReq, ToolKind};

    fn set() -> Arc<PluginSet> {
        plugin_set()
    }

    /// A shipped build must not carry a definition it cannot read. This is the
    /// test that makes the run-time error arm above unreachable.
    #[test]
    fn the_embedded_manifests_all_load() {
        let s = set();
        assert!(
            s.errors.is_empty(),
            "an embedded manifest did not load: {:?}",
            s.errors.iter().map(|e| &e.reason).collect::<Vec<_>>()
        );
        assert_eq!(s.plugins.len(), EMBEDDED.len());
        assert!(s.plugins.iter().all(|p| p.provenance == Provenance::Builtin));
    }

    /// The roster is fourteen tools under one key, and both halves matter: the
    /// count is the milestone's claim, and the key is a settings key the
    /// migration writes by hand.
    #[test]
    fn the_audit_plugin_is_the_fourteen_tools_under_a_stable_key() {
        let s = set();
        let p = s
            .plugins
            .iter()
            .find(|p| p.key == AUDIT_PLUGIN_KEY)
            .expect("the built-in audit plugin");
        assert_eq!(
            p.manifest.tools.len(),
            14,
            "the built-in audit roster is fourteen tools"
        );
        // The three that were the V23 Security trio must still be `security`
        // KIND — the security floor keys off kind and provenance, never off the
        // category a manifest files them under.
        for id in ["osv-scanner", "gitleaks", "semgrep"] {
            let t = p
                .manifest
                .tools
                .iter()
                .find(|t| t.id == id)
                .unwrap_or_else(|| panic!("no built-in tool `{id}`"));
            assert_eq!(t.kind, ToolKind::Security, "{id}");
        }
        assert!(
            p.manifest
                .tools
                .iter()
                .filter(|t| t.kind == ToolKind::Security)
                .count()
                == 3,
            "exactly the trio is security-kind"
        );
    }

    /// Every built-in tool must be resolvable without the user configuring a
    /// path, must keep the pre-V38 output gate, and must keep the pre-V38
    /// "degrade loudly" sandbox posture. All three are regressions if missed,
    /// and all three are one field each — exactly the kind of thing a
    /// fourteen-entry file loses silently.
    #[test]
    fn every_builtin_tool_declares_the_facts_the_migration_depends_on() {
        let s = set();
        for p in &s.plugins {
            for t in &p.manifest.tools {
                assert!(
                    t.command.as_deref().is_some_and(|c| !c.is_empty()),
                    "built-in tool `{}` declares no `command`, so it would be inert until the \
                     user found and typed a path for it — which is the upgrade regression this \
                     field exists to prevent",
                    t.id
                );
                assert_eq!(
                    t.ingest,
                    Some(IngestReq::Grandfathered),
                    "built-in tool `{}` must keep the output gate its behaviour was measured \
                     against; the strict gate would turn a clean gitleaks run (which writes no \
                     report at all) into a tool failure",
                    t.id
                );
                assert_eq!(
                    t.sandbox,
                    SandboxReq::Optional,
                    "built-in tool `{}` must keep degrading loudly rather than refusing: \
                     `required` would stop every audit on a machine with the sandbox off, which \
                     is the current default",
                    t.id
                );
                assert!(
                    !t.label.trim().is_empty() && t.description.is_some(),
                    "built-in tool `{}` should say what it is for — the settings list is the \
                     only place a user meets it",
                    t.id
                );
            }
        }
    }

    /// The two tools that must NOT run on a fresh install, by name. Losing
    /// either default would make a first quality audit run a real .NET build or
    /// fetch a ruleset over the network without anyone asking for it.
    #[test]
    fn the_heavyweight_tools_stay_opt_in() {
        let s = set();
        let p = &s.plugins[0];
        let off: Vec<&str> = p
            .manifest
            .tools
            .iter()
            .filter(|t| !t.enabled_by_default)
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(off, vec!["dotnet-analyzers", "semgrep-quality"]);
    }

    /// A user plugin may not claim the reserved prefix, and the loader stamps
    /// provenance rather than reading it — so the same bytes, scanned, are a
    /// user plugin and are refused. Pinned here because it is the property the
    /// whole built-in/user distinction rests on.
    #[test]
    fn the_same_bytes_scanned_would_be_refused_as_a_user_plugin() {
        for (_, text) in EMBEDDED {
            let err = manifest::parse(text, Provenance::User)
                .expect_err("an embedded manifest must not load as a user plugin");
            assert!(
                matches!(err.error, manifest::ValidationError::ReservedName(_)),
                "expected the reserved-prefix refusal, got {err}"
            );
        }
    }
}

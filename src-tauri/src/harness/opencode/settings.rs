//! V40 Phase B — **OpenCode's own settings**: the fields core stores opaquely
//! in `Settings::harness["opencode"].ext` and never names.
//!
//! Locked decision 6. Three core `Settings` fields moved here, and each was a
//! different flavour of the same defect:
//!
//! * `offload.opencode_provider_auto` — a `bool` in the *offload* block whose
//!   entire meaning is "keep OpenCode's `local-llama` provider in step".
//! * `offload.opencode_provider` — the derived provider block itself. cImp
//!   writes it (the Offload section's *Add to OpenCode* button, or the
//!   auto-sync above); the user never types it. That is what
//!   [`SettingKind::Json`] is for: stored and round-tripped, validated only as
//!   "null or an object", and NOT rendered by the generic form.
//! * `offload.injection.opencode_native_gate_enabled` — the **L2** of an
//!   injection feature whose mechanism is a `tool.execute.before` handler
//!   inside *this harness's* generated plugin. Core held an app-wide flag for a
//!   control that could only ever reach one harness; the feature is
//!   harness-scoped now (`injection::Feature::scoped_harnesses`, derived from
//!   `HarnessPlugin::scoped_features`) and its L2 is the row below.
//!
//! All three are `spawn_baked`, so all three ride this harness's spawn
//! signature automatically and a flip raises a restart hint naming OpenCode
//! only.

use crate::harness::plugin::{SettingDefault, SettingField, SettingKind};
use crate::settings::{OpencodeLocalProvider, Settings};

/// `ext` key: the native-tool gate's app-wide L2 (was
/// `offload.injection.opencode_native_gate_enabled`).
pub const NATIVE_GATE: &str = "native_gate";
/// `ext` key: keep [`PROVIDER`] in step with the primary Local backend's
/// command (was `offload.opencode_provider_auto`).
pub const PROVIDER_AUTO: &str = "provider_auto";
/// `ext` key: the derived `local-llama` provider block, or `null` (was
/// `offload.opencode_provider`).
pub const PROVIDER: &str = "provider";

/// OpenCode's declared settings.
pub const FIELDS: &[SettingField] = &[
    SettingField {
        key: NATIVE_GATE,
        kind: SettingKind::Bool,
        label: "Native-tool gating",
        hint: "Let the generated OpenCode plugin DENY the harness's own native tools against \
               the tab's taint latch, rather than only beaconing on them. App-wide ceiling; \
               each tab opts in from its shield badge.",
        default: SettingDefault::Bool(true),
        spawn_baked: true,
        secret: false,
    },
    SettingField {
        key: PROVIDER_AUTO,
        kind: SettingKind::Bool,
        label: "Keep the local-llama provider in sync",
        hint: "Re-derive the injected provider block from the primary Local backend's command \
               at each launch and on save, so editing the command takes effect without \
               re-clicking Add to OpenCode.",
        default: SettingDefault::Bool(false),
        spawn_baked: true,
        secret: false,
    },
    SettingField {
        key: PROVIDER,
        kind: SettingKind::Json,
        label: "Derived local-llama provider",
        hint: "Written by cImp, not typed: the provider block injected into \
               OPENCODE_CONFIG_CONTENT.",
        default: SettingDefault::Null,
        spawn_baked: true,
        secret: false,
    },
];

/// The stored provider snapshot, or `None`.
fn stored_provider(s: &Settings) -> Option<OpencodeLocalProvider> {
    let v = s.harness_ext(super::harness_plugin::me(), PROVIDER);
    if v.is_null() {
        return None;
    }
    serde_json::from_value(v).ok()
}

/// Whether the auto-sync switch is on AND the offload server is enabled — the
/// two halves of the "auto" contract, read in one place.
fn auto_sync_active(s: &Settings) -> bool {
    s.harness_ext_bool(super::harness_plugin::me(), PROVIDER_AUTO) && s.offload.enabled
}

/// The effective `local-llama` provider to inject into an OpenCode session, or
/// `None` to inject nothing.
///
/// Moved verbatim from `OffloadSettings::resolve_opencode_provider` in V40
/// Phase B: when auto-sync is on and the local server is enabled, re-derive
/// from the current primary Local command so edits take effect at launch
/// without re-clicking the button; if that command is missing/incomplete, fall
/// back to the last persisted snapshot. Otherwise use the stored snapshot
/// as-is.
pub fn resolve_provider(s: &Settings) -> Option<OpencodeLocalProvider> {
    if auto_sync_active(s) {
        if let Some(cmd) = s.offload.primary_local_command() {
            if let Ok(p) = crate::offload::server::derive_opencode_provider(&cmd) {
                return Some(p);
            }
        }
    }
    stored_provider(s)
}

/// Re-sync the persisted `local-llama` snapshot on a settings save.
///
/// Moved verbatim from `OffloadSettings::sync_opencode_provider_on_save`. No-op
/// unless auto-sync is on AND the local server is enabled (per the auto
/// contract: disabled ⇒ do nothing). Re-derives only when the primary Local
/// command differs from the snapshot's `source_command`, so unrelated saves
/// don't churn. A derive failure (missing `--port`/model) leaves the prior
/// snapshot untouched rather than clearing it.
pub fn sync_provider_on_save(s: &mut Settings) {
    if !auto_sync_active(s) {
        return;
    }
    let Some(cmd) = s.offload.primary_local_command() else {
        return;
    };
    let unchanged = stored_provider(s).is_some_and(|p| p.source_command == cmd);
    if unchanged {
        return;
    }
    if let Ok(p) = crate::offload::server::derive_opencode_provider(&cmd) {
        if let Ok(v) = serde_json::to_value(&p) {
            s.set_harness_ext(super::harness_plugin::me(), PROVIDER, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{OffloadBackend, OffloadBackendKind};

    fn local_backend(cmd: &str) -> OffloadBackend {
        OffloadBackend {
            name: "local".to_string(),
            enabled: true,
            kind: OffloadBackendKind::Local {
                server_command: cmd.to_string(),
                autostart: false,
                show_command_on_start: false,
                auth_token: String::new(),
            },
            ..OffloadBackend::default()
        }
    }

    const CMD: &str = "llama-server -a first --port 8080";
    const CMD2: &str = "llama-server -a second --port 9099";

    fn settings_with(cmd: &str, enabled: bool, auto: bool) -> Settings {
        let mut s = Settings::default();
        s.offload.enabled = enabled;
        s.offload.backends = vec![local_backend(cmd)];
        s.set_harness_ext(
            super::super::harness_plugin::me(),
            PROVIDER_AUTO,
            serde_json::Value::Bool(auto),
        );
        s
    }

    /// The three no-op arms of the auto contract, each for its own reason.
    #[test]
    fn the_sync_is_a_no_op_unless_both_halves_of_the_auto_contract_hold() {
        // Auto off.
        let mut s = settings_with(CMD, true, false);
        sync_provider_on_save(&mut s);
        assert!(stored_provider(&s).is_none(), "auto off ⇒ no sync");

        // Auto on, server disabled.
        let mut s = settings_with(CMD, false, true);
        sync_provider_on_save(&mut s);
        assert!(stored_provider(&s).is_none(), "disabled server ⇒ no sync");
    }

    /// A first sync writes the snapshot; a second with the same command does
    /// not churn; a changed command re-derives.
    #[test]
    fn the_sync_writes_once_and_follows_the_command() {
        let mut s = settings_with(CMD, true, true);
        sync_provider_on_save(&mut s);
        assert_eq!(stored_provider(&s).expect("derived").model, "first");

        let snap = s.harness_ext(super::super::harness_plugin::me(), PROVIDER);
        sync_provider_on_save(&mut s);
        assert_eq!(
            s.harness_ext(super::super::harness_plugin::me(), PROVIDER),
            snap,
            "no change ⇒ no churn"
        );

        s.offload.backends = vec![local_backend(CMD2)];
        sync_provider_on_save(&mut s);
        assert_eq!(stored_provider(&s).expect("re-derived").model, "second");
    }

    /// The whole point of the move: the provider is reachable through the
    /// harness map, with no `offload` field naming a harness.
    #[test]
    fn resolve_reads_the_stored_snapshot_when_auto_is_off() {
        let mut s = settings_with(CMD, true, true);
        sync_provider_on_save(&mut s);
        s.set_harness_ext(
            super::super::harness_plugin::me(),
            PROVIDER_AUTO,
            serde_json::Value::Bool(false),
        );
        // Auto off: the stored snapshot stands even though the command moved.
        s.offload.backends = vec![local_backend(CMD2)];
        assert_eq!(resolve_provider(&s).expect("snapshot").model, "first");
    }
}

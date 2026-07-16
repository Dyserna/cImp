//! Settings file migrations to the v1.2 (V3-design) shape.
//!
//! Operates on untyped `serde_json::Value` so each version's transformation
//! can run as a pre-pass before serde's typed deserialize. Two paths are
//! supported:
//!
//!   - v1 → v1.2: a settings file from before the v2 design (had a top-level
//!     `claude_code` object, no `tabs`). Rare in practice — every shipped
//!     build since v1.1 migrated such files on launch — but we keep the
//!     path alive so a long-dormant install still upgrades cleanly.
//!   - v1.1 → v1.2: the v2-design shape (`tabs.{claude,aider}` object,
//!     `_shell_1_tmp` interim key) into the v3 array shape with reserved
//!     ids and a default Shell tab.
//!
//! The schema discriminator (`tabs` field shape) makes the v1.1 → v1.2
//! transform idempotent: once `tabs` is an array, the function is a no-op.
//!
//! Backups are written with collision-rotation so a user who somehow rolls
//! back and re-migrates doesn't lose the original.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::settings::schema::{default_claude_tab, CLAUDE_TAB_ID, SHELL_DEFAULT_TAB_ID};
use crate::shell::ShellSpec;

/// V1.4-07: the aider tab id is gone from the live schema, but the
/// v1 / v1.1 → v1.2 migrations still produce intermediate JSON that
/// includes an "aider" entry — that entry is rewritten in place by
/// the v1.7 → v1.8 step further along the cascade. We keep the
/// literal here so the early migrations don't depend on a constant
/// that no longer exists in the live schema.
const LEGACY_AIDER_TAB_ID: &str = "aider";

/// V1.4-07: produces a v1.2-shape aider tab JSON object. Used only by
/// the v1 → v1.2 and v1.1 → v1.2 migration paths when the source file
/// had no aider section to carry forward. The resulting object has the
/// fields the v1.2 schema required at the time (notably `ai_tool_kind`,
/// which was dropped from the live schema in v1.8); subsequent
/// migrations transform it through to v1.8 along with everything else.
fn legacy_aider_v1_2_entry() -> Value {
    json!({
        "kind": "ai_tool",
        "id": LEGACY_AIDER_TAB_ID,
        "ai_tool_kind": "aider",
        "builtin": true,
        "name": "Aider",
        "command": "aider",
        "args": [],
        "cwd": null,
        "env": {},
        "tts_injection": { "enabled": false, "instructions": "" },
        "notifications": {
            "idle": "Aider is idle",
            "awaiting_permission": "Aider is awaiting permission",
            "error": "Aider encountered an error",
        },
        "first_launch_notice_dismissed": false,
    })
}

/// Detect file shape and run the appropriate transform on `value`. Returns
/// `Ok(true)` if the file changed shape (caller should write back to disk),
/// `Ok(false)` if the file was already v1.2, or `Err` if a backup write
/// failed. Backup-write failure aborts migration loudly — we never proceed
/// without a recoverable copy.
pub fn migrate_if_needed(
    value: &mut Value,
    path: &Path,
    default_shell: &ShellSpec,
) -> AppResult<bool> {
    // Detect the entry-point version once, write *one* backup (named
    // after the entry version), then run every subsequent step without
    // its own backup. Pre-V0.6 each step wrote its own `.bak` file, so a
    // v1.0 file produced seven stamped backups on a single launch
    // (`v1.0.bak`, `v1.2.bak`, `v1.3.bak`, …). One backup per cascade is
    // both less disk noise and a more accurate label — the entry version
    // is what the user originally had.
    let entry = detect_entry_version(value);
    if entry.is_none() {
        return Ok(false);
    }
    // A failed backup is NOT fatal: the user's settings are intact in `value`,
    // and aborting here would make the caller fall back to blank defaults for the
    // whole session (and, with a persistent cause like an AV file lock, every
    // session). Log it and migrate anyway — losing the safety backup is far less
    // harmful than discarding valid settings.
    if let Err(e) = write_backup(path, entry.unwrap(), value) {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "settings migration: backup write failed; proceeding without a backup"
        );
    }

    // Walk the dispatcher table once. Each step's `detect` is gated on
    // the previous step's marker being present, so a value entering the
    // cascade at v1.0 cleanly cascades through every subsequent step on
    // the same pass; a value already at v1.10 short-circuits because no
    // step's detector matches.
    for step in MIGRATION_STEPS {
        if (step.detect)(value) {
            (step.transform)(value, default_shell);
        }
    }

    // Fixpoint guard. A correct cascade leaves the value stamped at the current
    // schema. If a future detector-ordering mistake leaves it under-migrated,
    // surface it loudly AND force-stamp to current so the file isn't re-detected
    // and re-migrated (regenerating a backup) on every launch — the exact
    // unbounded-`.bak`-growth failure mode the schema_version was added to
    // prevent. The caller's typed parse still validates the shape and
    // quarantines if it is actually broken; the error log flags the bug for
    // repair. (Just logging here, as before, left `Ok(true)` writing the
    // stale-versioned file straight back into the re-migrate loop.)
    let current = crate::settings::schema::CURRENT_SCHEMA_VERSION;
    let final_version = value.get("schema_version").and_then(|v| v.as_u64());
    if final_version != Some(current as u64) {
        tracing::error!(
            ?final_version,
            expected = current,
            "settings migration: cascade did not reach the current schema version; \
             force-stamping to stop a re-migrate loop"
        );
        if let Some(obj) = value.as_object_mut() {
            obj.insert("schema_version".into(), serde_json::json!(current));
        }
    }

    Ok(true)
}

/// Pick the lowest-version detector that matches `value`. Returns `None`
/// when the file is already at the current schema (no migration needed).
/// Used by `migrate_if_needed` to label the single pre-cascade backup.
fn detect_entry_version(value: &Value) -> Option<&'static str> {
    MIGRATION_STEPS
        .iter()
        .find(|step| (step.detect)(value))
        .map(|step| step.from_version)
}

/// One step of the migration cascade: detect a particular legacy shape
/// and transform it forward by one schema version. The cascade is
/// declarative — adding a new schema bump is a single new entry in
/// `MIGRATION_STEPS`. Transform signatures are uniform `(value,
/// default_shell)`; steps that don't need the shell take it as an
/// underscore-prefixed param.
struct MigrationStep {
    from_version: &'static str,
    detect: fn(&Value) -> bool,
    transform: fn(&mut Value, &ShellSpec),
}

/// The cascade. Order matters: the v1.0 → v1.2 transform produces the
/// shape that the v1.2 → v1.3 detector needs to match, and so on. Two
/// entry points (v1.0 and v1.1) both produce v1.2; once any one of them
/// runs, the next pass looks for v1.2's discriminator.
///
/// V1.0 and V1.1 are the only steps that need the default shell (for
/// the `_shell_1_tmp` interim key). The rest accept it via the uniform
/// signature and ignore it.
const MIGRATION_STEPS: &[MigrationStep] = &[
    MigrationStep {
        from_version: "v1.0",
        detect: looks_v1,
        transform: migrate_v1_to_v1_2,
    },
    MigrationStep {
        from_version: "v1.1",
        detect: looks_v1_1,
        transform: migrate_v1_1_to_v1_2,
    },
    MigrationStep {
        from_version: "v1.2",
        detect: looks_v1_2,
        transform: migrate_v1_2_to_v1_3_step,
    },
    MigrationStep {
        from_version: "v1.3",
        detect: looks_v1_3,
        transform: migrate_v1_3_to_v1_4_step,
    },
    MigrationStep {
        from_version: "v1.4",
        detect: looks_v1_4,
        transform: migrate_v1_4_to_v1_5_step,
    },
    MigrationStep {
        from_version: "v1.5",
        detect: looks_v1_5,
        transform: migrate_v1_5_to_v1_6_step,
    },
    MigrationStep {
        from_version: "v1.6",
        detect: looks_v1_6,
        transform: migrate_v1_6_to_v1_7_step,
    },
    MigrationStep {
        from_version: "v1.7",
        detect: looks_v1_7,
        transform: migrate_v1_7_to_v1_8_step,
    },
    MigrationStep {
        from_version: "v1.8",
        detect: looks_v1_8,
        transform: migrate_v1_8_to_v1_9_step,
    },
    MigrationStep {
        from_version: "v1.9",
        detect: looks_v1_9,
        transform: migrate_v1_9_to_v1_10_step,
    },
    MigrationStep {
        from_version: "v1.10",
        detect: looks_v1_10,
        transform: migrate_v1_10_to_v1_11_step,
    },
    MigrationStep {
        from_version: "v1.11",
        detect: looks_v1_11,
        transform: migrate_v1_11_to_v1_12_step,
    },
    MigrationStep {
        from_version: "v1.12",
        detect: looks_v1_12,
        transform: migrate_v1_12_to_v1_13_step,
    },
    MigrationStep {
        from_version: "v1.13",
        detect: looks_v1_13,
        transform: migrate_v1_13_to_v1_14_step,
    },
    MigrationStep {
        from_version: "v1.14",
        detect: looks_v1_14,
        transform: migrate_v1_14_to_v1_15_step,
    },
    MigrationStep {
        from_version: "v1.15",
        detect: looks_v1_15,
        transform: migrate_v1_15_to_v1_16_step,
    },
    MigrationStep {
        from_version: "v1.16",
        detect: looks_v1_16,
        transform: migrate_v1_16_to_v1_17_step,
    },
    MigrationStep {
        from_version: "v1.17",
        detect: looks_v1_17,
        transform: migrate_v1_17_to_v1_18_step,
    },
    MigrationStep {
        from_version: "v18",
        detect: looks_v18,
        transform: migrate_v18_to_v19_step,
    },
    MigrationStep {
        from_version: "v19",
        detect: looks_v19,
        transform: migrate_v19_to_v20_step,
    },
    MigrationStep {
        from_version: "v20",
        detect: looks_v20,
        transform: migrate_v20_to_v21_step,
    },
    MigrationStep {
        from_version: "v21",
        detect: looks_v21,
        transform: migrate_v21_to_v22_step,
    },
    MigrationStep {
        from_version: "v22",
        detect: looks_v22,
        transform: migrate_v22_to_v23_step,
    },
    MigrationStep {
        from_version: "v23",
        detect: looks_v23,
        transform: migrate_v23_to_v24_step,
    },
    MigrationStep {
        from_version: "v24",
        detect: looks_v24,
        transform: migrate_v24_to_v25_step,
    },
];

// --- Uniform-signature wrappers -------------------------------------------
//
// The MIGRATION_STEPS table requires every transform to have the same
// signature `fn(&mut Value, &ShellSpec)`. The v1.0 and v1.1 transforms
// natively use the shell; the rest don't, and we expose a thin shim
// here to keep the table mechanical and the underlying transforms
// clean of unused parameters.

fn migrate_v1_2_to_v1_3_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_2_to_v1_3(value)
}
fn migrate_v1_3_to_v1_4_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_3_to_v1_4(value)
}
fn migrate_v1_4_to_v1_5_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_4_to_v1_5(value)
}
fn migrate_v1_5_to_v1_6_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_5_to_v1_6(value)
}
fn migrate_v1_6_to_v1_7_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_6_to_v1_7(value)
}
fn migrate_v1_7_to_v1_8_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_7_to_v1_8(value)
}
fn migrate_v1_8_to_v1_9_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_8_to_v1_9(value)
}
fn migrate_v1_9_to_v1_10_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_9_to_v1_10(value)
}
fn migrate_v1_10_to_v1_11_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_10_to_v1_11(value)
}
fn migrate_v1_11_to_v1_12_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_11_to_v1_12(value)
}
fn migrate_v1_12_to_v1_13_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_12_to_v1_13(value)
}
fn migrate_v1_13_to_v1_14_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_13_to_v1_14(value)
}
fn migrate_v1_14_to_v1_15_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_14_to_v1_15(value)
}
fn migrate_v1_15_to_v1_16_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_15_to_v1_16(value)
}
fn migrate_v1_16_to_v1_17_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_16_to_v1_17(value)
}
fn migrate_v1_17_to_v1_18_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v1_17_to_v1_18(value)
}

fn looks_v1(value: &Value) -> bool {
    value.get("claude_code").is_some() && value.get("tabs").is_none()
}

fn looks_v1_1(value: &Value) -> bool {
    matches!(value.get("tabs"), Some(Value::Object(_)))
}

/// Is this a v1.2 file (post-v1.2 tabs-array shape) that lacks the v1.3
/// `layout` field entirely? Triggers only when the `layout` key is
/// absent from the top-level object — files that already have
/// `"layout": null` (fresh-install defaults written by v1.3) skip
/// re-migration so the user doesn't accumulate `.v1.2.bak.<ts>` files
/// on every launch.
fn looks_v1_2(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    // `schema_version` is first stamped at the v1.9 → v1.10 step, so a
    // genuine pre-v1.10 file never carries it. Guarding on its absence
    // stops this presence-archaeology predicate from false-positiving on a
    // modern (v1.10+) file that merely lacks the `layout` key — which would
    // otherwise drag it back through the entire cascade and write a fresh
    // `.v1.2.bak.<ts>` on every launch (unbounded backup growth).
    if obj.contains_key("schema_version") {
        return false;
    }
    let tabs_is_array = obj.get("tabs").map(|v| v.is_array()).unwrap_or(false);
    let layout_field_absent = !obj.contains_key("layout");
    tabs_is_array && layout_field_absent
}

/// Is this a v1.3 file that lacks the v1.4 `terminal` field? V1.4-01's
/// schema-bump check. The `layout` key (added by the v1.2→v1.3 branch
/// above) must be present so we don't false-positive on v1.0/v1.1/v1.2
/// inputs already on their way through the cascade — those land in
/// `looks_v1_2` first, get rewritten with a `layout`, then this branch
/// fires on the same value before flush. A fresh-install v1.4 file has
/// `terminal` populated and skips this branch.
fn looks_v1_3(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.contains_key("schema_version") {
        return false;
    }
    obj.contains_key("layout") && !obj.contains_key("terminal")
}

/// Is this a v1.4 file that lacks the v1.5 `terminal.background` field?
/// V1.4-02's schema-bump check. `terminal` must be present (else we
/// haven't run the v1.4 step yet); `terminal.background` must be absent.
/// A fresh-install v1.5 file has `terminal.background` populated and
/// skips this branch — the `serde(default)` impl on
/// `TerminalBackgroundSettings` would also tolerate the absence on
/// load, but we want the on-disk file to be self-describing and the
/// pre-migration backup to exist.
fn looks_v1_4(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.contains_key("schema_version") {
        return false;
    }
    let has_terminal = obj.contains_key("terminal");
    let has_terminal_background = obj
        .get("terminal")
        .and_then(|t| t.get("background"))
        .is_some();
    has_terminal && !has_terminal_background
}

/// Is this a v1.5 file that lacks the v1.6
/// `terminal.background.presets` field? V1.4-04 B's schema-bump check.
/// `terminal.background` must be present (else we haven't run the v1.5
/// step yet); `terminal.background.presets` must be absent. Fresh-install
/// v1.6 files have `presets: []` populated and skip this branch.
fn looks_v1_5(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.contains_key("schema_version") {
        return false;
    }
    let bg = obj.get("terminal").and_then(|t| t.get("background"));
    match bg {
        Some(b) => b.get("presets").is_none(),
        None => false,
    }
}

/// Is this a v1.6 file that lacks the v1.7 `terminal.scrollback` field?
/// V1.4-04 D's schema-bump check. `terminal.background.presets` must
/// be present (so we don't false-positive on earlier files mid-cascade);
/// `terminal.scrollback` must be absent. A fresh-install v1.7 file has
/// `terminal.scrollback` populated and skips this branch.
fn looks_v1_6(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.contains_key("schema_version") {
        return false;
    }
    let has_presets = obj
        .get("terminal")
        .and_then(|t| t.get("background"))
        .and_then(|bg| bg.get("presets"))
        .is_some();
    let has_scrollback = obj
        .get("terminal")
        .and_then(|t| t.get("scrollback"))
        .is_some();
    has_presets && !has_scrollback
}

/// Is this a v1.7 file that lacks the v1.8 `claude_local` field? V1.4-07's
/// schema-bump check. `terminal.scrollback` must be present (so we don't
/// false-positive on earlier files mid-cascade); `claude_local` must be
/// absent. Fresh-install v1.8 files have `claude_local` populated and
/// skip this branch.
fn looks_v1_7(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.contains_key("schema_version") {
        return false;
    }
    let has_scrollback = obj
        .get("terminal")
        .and_then(|t| t.get("scrollback"))
        .is_some();
    let has_claude_local = obj.contains_key("claude_local");
    has_scrollback && !has_claude_local
}

/// Is this a v1.8 file that lacks the v1.9 `claude_tabs_enabled` field?
/// `claude_local` must be present (v1.8 marker, so we don't false-positive
/// on earlier files mid-cascade); `claude_tabs_enabled` must be absent.
/// Fresh-install v1.9 files have `claude_tabs_enabled` populated and
/// skip this branch. The schema_version absence guard avoids
/// re-firing on V14+ files where `claude_tabs_enabled` was removed by
/// the v1.13 → v1.14 migration: a real v1.8 file has no
/// `schema_version` (v1.10 is what stamps it).
fn looks_v1_8(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    let has_claude_local = obj.contains_key("claude_local");
    let has_setting = obj.contains_key("claude_tabs_enabled");
    let has_version = obj.contains_key("schema_version");
    has_claude_local && !has_setting && !has_version
}

/// Is this a v1.9 file that lacks the v1.10 `schema_version` field? V1.10's
/// schema-bump check. `claude_tabs_enabled` must be present (v1.9 marker,
/// so we don't false-positive on earlier files mid-cascade);
/// `schema_version` must be absent. Fresh-install v1.10 files have
/// `schema_version: 10` populated and skip this branch.
///
/// Once every supported entry point has been migrated through v1.10 and
/// the `schema_version` field is universal, future detectors can simplify
/// to `value.get("schema_version") == Some(N)` instead of the
/// presence-of-key archaeology that the earlier `looks_v1_X` predicates
/// rely on. See `docs/features/FEATURE-secret-storage.md` for the
/// migration-style consolidation plan.
fn looks_v1_9(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    let has_setting = obj.contains_key("claude_tabs_enabled");
    let has_version = obj.contains_key("schema_version");
    has_setting && !has_version
}

/// Is this a v1.10 file (schema_version == 10) whose tab notifications
/// still use the bare-string per-slot shape? V1.11 promotes each slot
/// to `{ enabled, text }`; the predicate gates on the explicit
/// `schema_version` integer so a freshly-stamped v1.10 file can be
/// distinguished from a v1.11 one (which carries `schema_version: 11`).
/// Tolerant `Deserialize` on the slot type accepts both shapes, but the
/// migration still rewrites in place so the on-disk file reflects the
/// new shape immediately rather than only after the user's first save.
fn looks_v1_10(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 10)
}

/// Is this a v1.11 file (schema_version == 11) whose `avatar.margin_px`
/// scalar still needs to be promoted to the v1.12
/// `avatar.margin: { x_px, y_px }` object? Tolerant `Deserialize` on
/// `AvatarMargin` would absorb the absence on load, but we still rewrite
/// in place so the on-disk file matches the new shape from the first
/// post-upgrade launch and the user's legacy margin value is preserved
/// across both axes (rather than silently dropping to the default).
fn looks_v1_11(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 11)
}

/// Is this a v1.12 file (schema_version == 12) whose `ui.theme` still
/// uses the pre-split `"tui"` value? V1.13 rewrites that value (now to
/// `"tui-orange"`); the predicate gates on the explicit `schema_version`
/// integer so a freshly-stamped v1.12 file gets caught.
fn looks_v1_12(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 12)
}

/// Is this a v1.13 file (schema_version == 13) that still uses the
/// tri-state `claude_tabs_enabled` setting and lacks the V14 list-shape
/// `enabled_ai_tabs` / `aider_local` fields?
fn looks_v1_13(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 13)
}

/// Is this a v1.14 file (schema_version == 14) that lacks the V15 default
/// `broot` tab? Gated purely on the version integer; the transform's own
/// presence check makes re-injection idempotent.
fn looks_v1_14(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 14)
}

/// Is this a v1.15 file (schema_version == 15) that may still carry the
/// retired auto-seeded `broot` tab? Gated purely on the version integer; the
/// transform's own `retain` makes broot removal idempotent.
fn looks_v1_15(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 15)
}

/// Is this a v1.16 file (schema_version == 16) that pre-dates the V8-02
/// offload backend pool? Gated purely on the version integer; the
/// transform's own check (skip if `offload.backends` already populated)
/// makes the migration idempotent.
fn looks_v1_16(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 16)
}

// --- v1.1 → v1.2 ------------------------------------------------------------

fn migrate_v1_1_to_v1_2(value: &mut Value, default_shell: &ShellSpec) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    let old_tabs = root.remove("tabs").unwrap_or(Value::Null);
    let shell_tmp = root.remove("_shell_1_tmp").unwrap_or(Value::Null);

    let mut new_tabs: Vec<Value> = Vec::with_capacity(3);

    let claude_entry = old_tabs
        .get("claude")
        .filter(|v| v.is_object())
        .map(|v| transform_ai_tool_entry(v, AiToolBuiltin::Claude))
        .unwrap_or_else(|| {
            serde_json::to_value(default_claude_tab()).expect("encode claude default")
        });
    new_tabs.push(claude_entry);

    let aider_entry = old_tabs
        .get("aider")
        .filter(|v| v.is_object())
        .map(|v| transform_ai_tool_entry(v, AiToolBuiltin::Aider))
        .unwrap_or_else(legacy_aider_v1_2_entry);
    new_tabs.push(aider_entry);

    let shell_entry = transform_shell_1_from_interim(&shell_tmp, default_shell);
    new_tabs.push(shell_entry);

    root.insert("tabs".to_string(), Value::Array(new_tabs));
}

#[derive(Clone, Copy)]
enum AiToolBuiltin {
    Claude,
    Aider,
}

impl AiToolBuiltin {
    fn id(self) -> &'static str {
        match self {
            AiToolBuiltin::Claude => CLAUDE_TAB_ID,
            AiToolBuiltin::Aider => LEGACY_AIDER_TAB_ID,
        }
    }

    fn ai_tool_kind(self) -> &'static str {
        match self {
            AiToolBuiltin::Claude => "claude_code",
            AiToolBuiltin::Aider => "aider",
        }
    }

    fn name(self) -> &'static str {
        match self {
            AiToolBuiltin::Claude => "Claude",
            AiToolBuiltin::Aider => "Aider",
        }
    }

    fn default_command(self) -> &'static str {
        match self {
            AiToolBuiltin::Claude => "claude",
            AiToolBuiltin::Aider => "aider",
        }
    }
}

/// Transform a v1.1 `tabs.{claude,aider}` object into a v1.2 array entry.
/// Carries the user-set `command`, collapses `extra_cli_flags` into `args`,
/// and brings `tts_injection`, `notifications`, and
/// `first_launch_notice_dismissed` through verbatim.
fn transform_ai_tool_entry(old: &Value, kind: AiToolBuiltin) -> Value {
    let command = old
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| kind.default_command())
        .to_string();

    let args: Vec<Value> = old
        .get("extra_cli_flags")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Carry these through wholesale — their shape is unchanged across the
    // schema bump (TtsInjection and the AI notification triplet).
    let tts_injection = old
        .get("tts_injection")
        .cloned()
        .unwrap_or_else(|| json!({ "enabled": false, "instructions": "" }));
    let notifications = old.get("notifications").cloned().unwrap_or_else(|| {
        json!({
            "idle": "",
            "awaiting_permission": "",
            "error": "",
        })
    });
    let first_launch_notice_dismissed = old
        .get("first_launch_notice_dismissed")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut entry = Map::new();
    entry.insert("kind".to_string(), Value::String("ai_tool".to_string()));
    entry.insert("id".to_string(), Value::String(kind.id().to_string()));
    entry.insert(
        "ai_tool_kind".to_string(),
        Value::String(kind.ai_tool_kind().to_string()),
    );
    entry.insert("builtin".to_string(), Value::Bool(true));
    entry.insert("name".to_string(), Value::String(kind.name().to_string()));
    entry.insert("command".to_string(), Value::String(command));
    entry.insert("args".to_string(), Value::Array(args));
    entry.insert("cwd".to_string(), Value::Null);
    entry.insert("env".to_string(), Value::Object(Map::new()));
    entry.insert("tts_injection".to_string(), tts_injection);
    entry.insert("notifications".to_string(), notifications);
    entry.insert(
        "first_launch_notice_dismissed".to_string(),
        Value::Bool(first_launch_notice_dismissed),
    );
    Value::Object(entry)
}

/// Transform a v1.1 `_shell_1_tmp` entry into a v1.2 Shell array entry.
/// Picks up the user-edited name and notification strings; the spawn
/// command/args fall back to the resolved platform default since v1.1 had
/// no per-Shell-tab spawn config (Shell-1's binary was hardcoded at runtime).
fn transform_shell_1_from_interim(shell_tmp: &Value, default_shell: &ShellSpec) -> Value {
    let name = shell_tmp
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("Shell 1")
        .to_string();

    let notifications = shell_tmp
        .get("notifications")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| {
            json!({
                "error": "Shell encountered an error",
                "exited": "Shell exited (code {code})",
            })
        });

    let mut entry = Map::new();
    entry.insert("kind".to_string(), Value::String("shell".to_string()));
    entry.insert(
        "id".to_string(),
        Value::String(SHELL_DEFAULT_TAB_ID.to_string()),
    );
    entry.insert("builtin".to_string(), Value::Bool(false));
    entry.insert("name".to_string(), Value::String(name));
    entry.insert(
        "command".to_string(),
        Value::String(default_shell.command.to_string_lossy().into_owned()),
    );
    entry.insert(
        "args".to_string(),
        Value::Array(
            default_shell
                .args
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    entry.insert("cwd".to_string(), Value::Null);
    entry.insert("env".to_string(), Value::Object(Map::new()));
    entry.insert("notifications".to_string(), notifications);
    Value::Object(entry)
}

// --- v1 → v1.2 --------------------------------------------------------------
//
// v1 had `claude_code: { extra_cli_args, claude_md_path }` and no `tabs`
// field. The only field worth carrying forward is `extra_cli_args` —
// `claude_md_path` was a CLAUDE.md file path that became obsolete with
// `--append-system-prompt`-driven TTS injection (see the v2 design notes).
//
// Rather than chain v1 → v1.1 → v1.2, we lift directly to v1.2: build the
// three-entry tabs array with claude carrying the v1 args, drop
// `claude_code`, and let the v1.1 → v1.2 branch be a no-op afterward.

fn migrate_v1_to_v1_2(value: &mut Value, default_shell: &ShellSpec) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    let old_args: Vec<Value> = root
        .get("claude_code")
        .and_then(|cc| cc.get("extra_cli_args"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // v1's `claude_code.claude_md_path` was a path to a CLAUDE.md file the
    // wrapper auto-loaded; v1.2+ uses `--append-system-prompt` instead and
    // doesn't carry the field forward. Log the dropped value so a user
    // upgrading from a long-dormant install can see what they need to
    // re-configure (the rolling log file rotates on retention so this is
    // their one signal that the field went away).
    if let Some(p) = root
        .get("claude_code")
        .and_then(|cc| cc.get("claude_md_path"))
        .and_then(Value::as_str)
    {
        if !p.is_empty() {
            tracing::warn!(
                claude_md_path = %p,
                "settings v1→v1.2: dropping claude_md_path; the runtime now uses --append-system-prompt"
            );
        }
    }
    root.remove("claude_code");

    // Synthesize a v1.1-shaped "claude" tab with the carried args, then
    // hand off to the v1.1 → v1.2 transformer. Aider gets defaults (v1
    // never had an aider section).
    let claude_v1_1 = json!({
        "command": "claude",
        "extra_cli_flags": old_args,
        "tts_injection": { "enabled": true },
        "notifications": {
            "idle": "Claude is idle",
            "awaiting_permission": "Claude is awaiting permission",
            "error": "Claude encountered an error",
        },
        "first_launch_notice_dismissed": true,
    });

    let claude_entry = transform_ai_tool_entry(&claude_v1_1, AiToolBuiltin::Claude);
    let aider_entry = legacy_aider_v1_2_entry();
    let shell_entry = transform_shell_1_from_interim(&Value::Null, default_shell);

    root.insert(
        "tabs".to_string(),
        Value::Array(vec![claude_entry, aider_entry, shell_entry]),
    );
}

// --- v1.2 → v1.3 ------------------------------------------------------------
//
// v1.3 introduces the layout tree: a recursive split/pane structure that
// replaces v1.2's "all tabs in one tab bar" with arbitrary multi-pane
// arrangements. Migration builds a single root pane containing every tab
// in their existing order, with the focused-pane's active tab set from
// the v1.2 `session.active_tab_id` (or the first tab if absent). Drops
// `session.active_tab_id` afterwards because the layout owns it now.

fn migrate_v1_2_to_v1_3(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    // Pull tab ids in document order. Tabs missing an `id` field are
    // skipped — the integrity check at load time will repair the file
    // by re-inserting the reserved-id builtins.
    let tab_ids: Vec<Value> = root
        .get("tabs")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("id").and_then(Value::as_str))
                .map(|s| Value::String(s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Pick the active tab: prefer v1.2's session.active_tab_id, fall
    // back to the first tab in order. `null` is fine when the tab list
    // is empty (defensive — integrity will repopulate it).
    let session_active = root
        .get("session")
        .and_then(|s| s.get("active_tab_id"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let first_tab_id = tab_ids
        .first()
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let active_tab_id = session_active.or(first_tab_id);

    let pane_id = format!("pane-{}", Uuid::new_v4());
    let mut pane = Map::new();
    pane.insert("type".to_string(), Value::String("pane".to_string()));
    pane.insert("id".to_string(), Value::String(pane_id.clone()));
    pane.insert("tab_ids".to_string(), Value::Array(tab_ids));
    pane.insert(
        "active_tab_id".to_string(),
        active_tab_id.map(Value::String).unwrap_or(Value::Null),
    );

    let mut layout = Map::new();
    layout.insert("tree".to_string(), Value::Object(pane));
    layout.insert("focused_pane_id".to_string(), Value::String(pane_id));

    root.insert("layout".to_string(), Value::Object(layout));
    root.entry("layout_presets".to_string())
        .or_insert(Value::Array(Vec::new()));

    // Drop the redundant session.active_tab_id; the layout owns it now.
    if let Some(session) = root.get_mut("session").and_then(Value::as_object_mut) {
        session.remove("active_tab_id");
    }
}

// --- v1.3 → v1.4 ------------------------------------------------------------

/// V1.4-01: add the `terminal.theme` group, stamp `theme_override: null`
/// on every existing tab, and drop the now-dead `display.theme` field
/// (the xterm.js construction in `terminals.ts` ignored it pre-V1.4-01;
/// the new `terminal.theme.name` supersedes it under a clearer name).
///
/// Idempotent on second pass because the inserted `terminal` key makes
/// `looks_v1_3` return false next time.
fn migrate_v1_3_to_v1_4(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    // Drop dead `display.theme`.
    if let Some(display) = root.get_mut("display").and_then(Value::as_object_mut) {
        display.remove("theme");
    }

    // Insert the `terminal.theme` group pinned to the "Default" palette.
    // This is deliberately NOT the `TerminalThemeSettings::default()` value
    // (which seeds *fresh* installs and has since changed): an upgrading v1.3
    // user must keep their pre-V1.4-01 appearance, which the "Default" palette
    // preserves. Do not "sync" this to the Default impl.
    root.insert(
        "terminal".to_string(),
        json!({
            "theme": {
                "name": "Default",
                "custom": null
            }
        }),
    );

    // Stamp `theme_override: null` on every existing tab so the on-disk
    // file is self-describing. `serde(default)` would cover the absence
    // anyway, but explicit fields make hand-editing less surprising.
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            if let Some(obj) = tab.as_object_mut() {
                obj.insert("theme_override".to_string(), Value::Null);
            }
        }
    }
}

// --- v1.4 → v1.5 ------------------------------------------------------------

/// V1.4-02: add `terminal.background` with defaults and stamp
/// `background_override: null` on every existing tab.
///
/// Defaults must match `TerminalBackgroundSettings::default()` in
/// `schema.rs` — keep these in sync. The schema's `Default` impl is
/// what callers see when the field is absent on disk; this migration
/// makes the same values explicit so the on-disk file is
/// self-describing and the v1.4 backup is honest.
///
/// Idempotent on second pass because the inserted
/// `terminal.background` key makes `looks_v1_4` return false next time.
fn migrate_v1_4_to_v1_5(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(terminal) = root.get_mut("terminal").and_then(Value::as_object_mut) {
        terminal.insert(
            "background".to_string(),
            json!({
                "image": null,
                "color": null,
                "opacity": 0.4,
                "blur": 0,
                "size": "cover",
                "position": "center"
            }),
        );
    }

    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            if let Some(obj) = tab.as_object_mut() {
                obj.insert("background_override".to_string(), Value::Null);
            }
        }
    }
}

// --- v1.5 → v1.6 ------------------------------------------------------------

/// V1.4-04 B: stamp `terminal.background.presets` with an empty array.
/// `snapshot_lines` is *not* stamped here even though it was added in
/// V1.4-04 A — that field rides serde-default on existing v1.5 files
/// per the milestone doc, so the on-disk shape only formally bumps
/// when presets land. A user who upgraded through V1.4-04 A first
/// already has `snapshot_lines` filled by serde-default; the next
/// settings flush writes it explicitly.
///
/// Idempotent on second pass because the inserted `presets` key makes
/// `looks_v1_5` return false next time.
fn migrate_v1_5_to_v1_6(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(bg) = root
        .get_mut("terminal")
        .and_then(Value::as_object_mut)
        .and_then(|t| t.get_mut("background"))
        .and_then(Value::as_object_mut)
    {
        bg.insert("presets".to_string(), json!([]));
        // V1.4-04 A.1's `snapshot_lines` field landed without a dedicated
        // schema bump and silently rode `serde(default)`. Stamp it
        // explicitly here so the on-disk file is self-describing —
        // matching the value `TerminalBackgroundSettings::default()`
        // produces. `entry().or_insert` preserves any hand-edited value.
        bg.entry("snapshot_lines".to_string())
            .or_insert(json!(2000));
    }
}

// --- v1.6 → v1.7 ------------------------------------------------------------

/// V1.4-04 D: stamp `terminal.scrollback` defaults and explicitly write
/// `terminal.background.preview_category_flips` (V1.4-04 C.4) so the
/// on-disk shape is self-describing for both fields landed in V1.4-04.
///
/// Defaults must match `ScrollbackSettings::default()` and
/// `TerminalBackgroundSettings::default().preview_category_flips`.
///
/// Idempotent on second pass because the inserted `scrollback` key
/// makes `looks_v1_6` return false next time. Stamping
/// `preview_category_flips` is also idempotent — `entry().or_insert`
/// only writes when the field is absent, so a hand-edited file that
/// already has the flag is preserved.
fn migrate_v1_6_to_v1_7(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    let Some(terminal) = root.get_mut("terminal").and_then(Value::as_object_mut) else {
        return;
    };

    terminal.insert(
        "scrollback".to_string(),
        json!({
            "ring_bytes": 262144,
            "persist": true,
            "restore_on_launch": true,
        }),
    );

    if let Some(bg) = terminal
        .get_mut("background")
        .and_then(Value::as_object_mut)
    {
        bg.entry("preview_category_flips")
            .or_insert(Value::Bool(true));
    }
}

// --- v1.7 → v1.8 ------------------------------------------------------------

/// V1.4-07: drop the aider tab kind, add the global `claude_local`
/// provider config, add the per-AI-tab `use_local_provider` flag, and
/// rewrite any aider tab in place to claude-local.
///
/// Three concerns per pass:
///
/// 1. Stamp `claude_local` defaults at the top level of the file.
/// 2. Walk every AI tab: drop `ai_tool_kind`, default `use_local_provider`
///    to false, then if the tab is the legacy aider tab (id == "aider"),
///    rewrite it (id, name, command, use_local_provider, tts_injection).
/// 3. Rewrite layout-tree references to `"aider"` everywhere they live —
///    `layout.tree`, every `layout_presets[].tree`, and
///    `session.active_tab_id` — so the integrity check sees a
///    self-consistent file.
///
/// Idempotent: a second pass finds `claude_local` present and
/// `looks_v1_7` returns false. The aider id rewrite is also a no-op
/// after the first pass since no tab carries it any more.
fn migrate_v1_7_to_v1_8(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    // 1. Top-level claude_local defaults.
    root.insert(
        "claude_local".to_string(),
        json!({
            "base_url": "http://localhost:4000",
            "auth_token": "sk-dummy",
            "model_alias": "",
        }),
    );

    // 2. Walk tabs.
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            let Some(obj) = tab.as_object_mut() else {
                continue;
            };
            let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind != "ai_tool" {
                continue;
            }

            let was_aider = obj.get("id").and_then(Value::as_str) == Some(LEGACY_AIDER_TAB_ID);

            // Drop the dead AI-tool discriminator (collapsed to a single
            // ClaudeCode kind in V1.4-07).
            obj.remove("ai_tool_kind");

            // Default the new flag (false; the rewrite below flips it
            // for the legacy aider tab).
            obj.entry("use_local_provider".to_string())
                .or_insert(Value::Bool(false));

            if was_aider {
                obj.insert("id".to_string(), Value::String("claude-local".to_string()));
                obj.insert(
                    "name".to_string(),
                    Value::String("Claude (local)".to_string()),
                );
                obj.insert("command".to_string(), Value::String("claude".to_string()));
                obj.insert("args".to_string(), Value::Array(Vec::new()));
                obj.insert("use_local_provider".to_string(), Value::Bool(true));
                // Aider's default had tts_injection.enabled=false; the
                // rewritten tab is Claude, which speaks. Ensure the object
                // exists before enabling — otherwise an aider tab serialized
                // without a `tts_injection` field would skip the enable
                // entirely, leaving the rewritten Claude (local) tab silent.
                let tts_entry = obj
                    .entry("tts_injection".to_string())
                    .or_insert_with(|| json!({}));
                if let Some(tts) = tts_entry.as_object_mut() {
                    tts.insert("enabled".to_string(), Value::Bool(true));
                }
                // Notification strings — replace any "Aider …" text with
                // the matching "Claude (local) …" text. If the user
                // customized the notifications, we leave their custom
                // text alone (only rewrite the canonical Aider strings).
                if let Some(notifs) = obj.get_mut("notifications").and_then(Value::as_object_mut) {
                    let canonical_aider = [
                        ("idle", "Aider is idle", "Claude (local) is idle"),
                        (
                            "awaiting_permission",
                            "Aider is awaiting permission",
                            "Claude (local) is awaiting permission",
                        ),
                        (
                            "error",
                            "Aider encountered an error",
                            "Claude (local) encountered an error",
                        ),
                    ];
                    for (field, from_text, to_text) in canonical_aider {
                        if notifs.get(field).and_then(Value::as_str) == Some(from_text) {
                            notifs.insert(field.to_string(), Value::String(to_text.to_string()));
                        }
                    }
                }
                // first_launch_notice_dismissed: pre-dismiss so the gone
                // banner doesn't fire. Aider users who upgraded to a
                // schema where this carried through will already have
                // some value; force it true (safe — the banner code is
                // gone anyway).
                obj.insert(
                    "first_launch_notice_dismissed".to_string(),
                    Value::Bool(true),
                );
            }
        }
    }

    // 3. Rewrite layout-tree id references.
    rewrite_aider_tab_ids(root);
}

/// Walk layout-tree-shaped JSON inside the settings root and rewrite
/// any `"aider"` tab-id reference to `"claude-local"`. Covers
/// `layout.tree`, every `layout_presets[].tree`, and
/// `session.active_tab_id`. Used only by `migrate_v1_7_to_v1_8`.
fn rewrite_aider_tab_ids(root: &mut Map<String, Value>) {
    fn rewrite_node(node: &mut Value) {
        let Some(obj) = node.as_object_mut() else {
            return;
        };
        if let Some(arr) = obj.get_mut("tab_ids").and_then(Value::as_array_mut) {
            for entry in arr.iter_mut() {
                if entry.as_str() == Some(LEGACY_AIDER_TAB_ID) {
                    *entry = Value::String("claude-local".to_string());
                }
            }
        }
        if obj.get("active_tab_id").and_then(Value::as_str) == Some(LEGACY_AIDER_TAB_ID) {
            obj.insert(
                "active_tab_id".to_string(),
                Value::String("claude-local".to_string()),
            );
        }
        if let Some(child) = obj.get_mut("first") {
            rewrite_node(child);
        }
        if let Some(child) = obj.get_mut("second") {
            rewrite_node(child);
        }
    }

    if let Some(layout) = root.get_mut("layout") {
        if let Some(tree) = layout.get_mut("tree") {
            rewrite_node(tree);
        }
    }
    if let Some(presets) = root.get_mut("layout_presets").and_then(Value::as_array_mut) {
        for preset in presets.iter_mut() {
            if let Some(obj) = preset.as_object_mut() {
                if let Some(tree) = obj.get_mut("tree") {
                    rewrite_node(tree);
                }
            }
        }
    }
    if let Some(session) = root.get_mut("session").and_then(Value::as_object_mut) {
        if session.get("active_tab_id").and_then(Value::as_str) == Some(LEGACY_AIDER_TAB_ID) {
            session.insert(
                "active_tab_id".to_string(),
                Value::String("claude-local".to_string()),
            );
        }
    }
}

// --- v1.8 → v1.9 ------------------------------------------------------------
//
// v1.9 introduces the `claude_tabs_enabled` setting (Cloud / Local /
// Both). The migration infers the initial value from the existing tabs
// array so users who had both Claude tabs in v1.8 keep both after the
// upgrade; users who had only one keep that one. Idempotent — a second
// pass finds the field present and `looks_v1_8` returns false.

fn migrate_v1_8_to_v1_9(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    let inferred = infer_claude_tabs_enabled(root);
    root.insert(
        "claude_tabs_enabled".to_string(),
        Value::String(inferred.to_string()),
    );
}

fn infer_claude_tabs_enabled(root: &Map<String, Value>) -> &'static str {
    let mut has_claude = false;
    let mut has_claude_local = false;
    if let Some(tabs) = root.get("tabs").and_then(Value::as_array) {
        for tab in tabs {
            match tab.get("id").and_then(Value::as_str) {
                Some("claude") => has_claude = true,
                Some("claude-local") => has_claude_local = true,
                _ => {}
            }
        }
    }
    match (has_claude, has_claude_local) {
        (true, true) => "both",
        (false, true) => "local",
        // (true, false) and (false, false) both default to cloud — for
        // (false, false), the integrity check will recreate the claude
        // tab on its next pass.
        _ => "cloud",
    }
}

// --- v1.9 → v1.10 -----------------------------------------------------------
//
// Adds the explicit `schema_version` integer field (default 10). Pre-V1.10
// files relied on presence-of-key archaeology to detect their version; this
// step plants the discriminator so future migrations can use a single
// integer comparison.

fn migrate_v1_9_to_v1_10(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    root.insert(
        "schema_version".to_string(),
        // V1.11 raised CURRENT_SCHEMA_VERSION; this step only promotes
        // the file to v1.10, so stamp the literal 10 rather than the
        // moving constant.
        Value::Number(serde_json::Number::from(10u8)),
    );
}

// --- v1.10 → v1.11 ----------------------------------------------------------
//
// Promotes each notification slot from a bare string to a
// `{ enabled, text }` object. The mapping mirrors the documented
// "leave blank to disable" convention: empty string → disabled,
// non-empty → enabled. The schema's tolerant `Deserialize` would
// transparently absorb the legacy shape on load, but rewriting on
// migration means the on-disk file matches the live shape from the
// first launch after upgrade rather than only after the user's first
// settings save.

fn migrate_v1_10_to_v1_11(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs {
            if let Some(notifications) = tab.get_mut("notifications").and_then(Value::as_object_mut)
            {
                let keys: Vec<String> = notifications.keys().cloned().collect();
                for key in keys {
                    if let Some(slot) = notifications.get_mut(&key) {
                        if let Value::String(text) = slot {
                            let promoted = json!({
                                "enabled": !text.is_empty(),
                                "text": text,
                            });
                            *slot = promoted;
                        }
                    }
                }
            }
        }
    }
    root.insert(
        "schema_version".to_string(),
        // V1.12 raised CURRENT_SCHEMA_VERSION; this step only promotes
        // the file to v1.11, so stamp the literal 11 rather than the
        // moving constant. The next step (v1.11 → v1.12) bumps to current.
        Value::Number(serde_json::Number::from(11u8)),
    );
}

// --- v1.11 → v1.12 ----------------------------------------------------------
//
// Promotes `avatar.margin_px: u32` to `avatar.margin: { x_px, y_px }`
// so the user can offset the avatar independently per axis. The legacy
// scalar value is copied to both fields, preserving the on-screen
// position for everyone who hadn't tweaked their margin since v1.11.
// Defaults (16) flow in via `AvatarMargin::default()` if the legacy
// field is absent.

fn migrate_v1_11_to_v1_12(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(avatar) = root.get_mut("avatar").and_then(Value::as_object_mut) {
        // Only convert when the legacy field was actually present. Inserting a
        // default `margin` when `margin_px` is absent pollutes the overlay (which
        // holds only diffs from global) with a phantom default that then
        // overrides global forever. Absent → let `AvatarMargin::default()` fill it.
        if let Some(legacy) = avatar.remove("margin_px").and_then(|v| v.as_u64()) {
            avatar.insert(
                "margin".to_string(),
                json!({
                    "x_px": legacy,
                    "y_px": legacy,
                }),
            );
        }
    }
    // V1.13 raised CURRENT_SCHEMA_VERSION; this step only promotes the
    // file to v1.12, so stamp the literal 12 rather than the moving
    // constant. The next step (v1.12 → v1.13) bumps to current.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(12u8)),
    );
}

// --- v1.12 → v1.13 ----------------------------------------------------------
//
// Splits the single `"tui"` UI theme into accent variants. The original
// `"tui-yellow"` / `"tui-purple"` variants this migration introduced have
// since been removed; existing `"tui"` strings are now rewritten to
// `"tui-orange"`, the surviving Gruvbox-surfaced theme, so those users keep
// the closest look to the old gruvbox `"tui"`.

fn migrate_v1_12_to_v1_13(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(ui) = root.get_mut("ui").and_then(Value::as_object_mut) {
        if let Some(Value::String(theme)) = ui.get_mut("theme") {
            if theme == "tui" {
                *theme = "tui-orange".to_string();
            }
        }
    }
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(13u8)),
    );
}

// --- v1.13 → v1.14 ----------------------------------------------------------
//
// V14 generalizes the v1.9 tri-state `claude_tabs_enabled` setting (Cloud /
// Local / Both) to a list of arbitrary AI-tab ids — so the user can also
// enable the new `aider` and `aider-local` builtins. The migration:
//
// 1. Translates the legacy `claude_tabs_enabled` string into the new
//    `enabled_ai_tabs` array and removes the old key.
// 2. Stamps default `aider_local` provider settings at the top level so
//    the field round-trips cleanly. (Aider tabs are not auto-added to
//    `tabs[]`; they materialize on first enable via the integrity check
//    or the lifecycle IPC.)
// 3. Bumps `schema_version` to 14.
//
// Idempotent — a second pass finds `enabled_ai_tabs` present and
// `looks_v1_13` returns false.

fn migrate_v1_13_to_v1_14(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    let enabled = match root
        .remove("claude_tabs_enabled")
        .as_ref()
        .and_then(Value::as_str)
    {
        Some("local") => vec![Value::String("claude-local".to_string())],
        Some("both") => vec![
            Value::String("claude".to_string()),
            Value::String("claude-local".to_string()),
        ],
        // "cloud" or anything unexpected: default to the cloud tab.
        _ => vec![Value::String("claude".to_string())],
    };
    root.insert("enabled_ai_tabs".to_string(), Value::Array(enabled));

    root.entry("aider_local".to_string()).or_insert(json!({
        "base_url": "http://localhost:11434/v1",
        "auth_token": "ollama",
        "model": "",
    }));

    // Stamp a *literal* 14 (not CURRENT_SCHEMA_VERSION): later steps in the
    // cascade gate on `schema_version == N`, so each step must stamp its own
    // concrete version. The final v14 → v15 step bumps to the current value.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(14u8)),
    );
}

// --- v1.14 → v1.15 ----------------------------------------------------------
//
// V15 ships a default `broot` tab (`broot -g`, launch-dir cwd). The
// migration injects the Shell-tab entry into existing files so upgraders
// get it too, then bumps `schema_version` to 15. The new tab id is left
// out of any persisted `layout` tree on purpose — the frontend's
// `validateAndRepairLayout` places tabs present in `tabs[]` but absent
// from the tree as "orphans" into the focused pane on next launch, so
// the tab shows up without this migration having to touch the layout.
//
// Idempotent: skips injection if a `shell-broot` entry already exists
// (e.g. a fresh-install file that was seeded with it, or a second pass),
// and `looks_v1_14` returns false once `schema_version` is 15.

fn migrate_v1_14_to_v1_15(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Array(tabs)) = root.get_mut("tabs") {
        let already_present = tabs.iter().any(|t| {
            t.get("id").and_then(Value::as_str) == Some(crate::settings::schema::SHELL_BROOT_TAB_ID)
        });
        if !already_present {
            tabs.push(json!({
                "kind": "shell",
                "id": crate::settings::schema::SHELL_BROOT_TAB_ID,
                // Reserved non-closable builtin (see SHELL_BROOT_TAB_ID).
                "builtin": true,
                "name": "broot",
                "command": "broot",
                "args": ["-g"],
                "cwd": null,
                "env": {},
                "notifications": {
                    "error": "Shell encountered an error",
                    "exited": "Shell exited (code {code})",
                },
            }));
        }
    }

    // Stamp a *literal* 15 (not CURRENT_SCHEMA_VERSION): the v15 → v16 step
    // gates on `schema_version == 15`, so this step must leave that concrete
    // value. (The broot tab injected just above is removed again by v15 → v16
    // — broot is no longer a persistent builtin — but the intermediate v15
    // shape is preserved so the cascade stays mechanical.)
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(15u8)),
    );
}

// --- v1.15 → v1.16 ----------------------------------------------------------
//
// V16 retires the auto-seeded `broot` builtin tab. broot is now launched on
// demand (like rustnet) from the bottom-bar tool buttons into ordinary
// closable Shell tabs, so the persistent `shell-broot` entry is dropped from
// existing files. The frontend's `validateAndRepairLayout` prunes the now-
// orphaned id from any persisted layout tree on next launch, so this
// migration only has to touch `tabs[]`.
//
// Idempotent: a file with no `shell-broot` entry is left unchanged (beyond
// the version stamp), and `looks_v1_15` returns false once `schema_version`
// is 16.

fn migrate_v1_15_to_v1_16(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Array(tabs)) = root.get_mut("tabs") {
        tabs.retain(|t| {
            t.get("id").and_then(Value::as_str) != Some(crate::settings::schema::SHELL_BROOT_TAB_ID)
        });
    }

    // Stamp a *literal* 16 (not CURRENT_SCHEMA_VERSION): the v16 → v17 step
    // gates on `schema_version == 16`, so this step must leave that concrete
    // value for the next detector to match.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(16u8)),
    );
}

// --- v1.16 → v1.17 ----------------------------------------------------------
//
// V8-02 generalizes the single local `llama-server` into a backend pool. The
// migration folds the V8-01 single-local config (`offload.server_command` +
// `offload.autostart`) into one `Local` entry in the new `offload.backends`
// array, so an upgrader's existing command keeps working as one
// quality-tier, all-tools local backend. The legacy scalar fields are left
// in place (still deserialized, and the runtime `effective_backends()`
// fallback also reads them) — this migration only *adds* the array so the
// on-disk file is self-describing and the user can edit the pool in Settings.
//
// Idempotent: skips synthesis if `offload.backends` is already a non-empty
// array (a fresh-install or re-migrated file), and `looks_v1_16` returns
// false once `schema_version` is 17.

fn migrate_v1_16_to_v1_17(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Object(offload)) = root.get_mut("offload") {
        let already = matches!(
            offload.get("backends"),
            Some(Value::Array(a)) if !a.is_empty()
        );
        if !already {
            let server_command = offload
                .get("server_command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let autostart = offload
                .get("autostart")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // Only synthesize a backend when there's an actual command to
            // carry forward; an empty/unconfigured offload block migrates to
            // an empty pool (the feature is still off by default).
            let backends = if server_command.trim().is_empty() {
                Value::Array(Vec::new())
            } else {
                json!([{
                    "name": "local",
                    "enabled": true,
                    "kind": { "type": "local", "server_command": server_command, "autostart": autostart },
                    "declared_context": null,
                    "declared_model": "",
                    "tier": "quality",
                    "tool_scope": { "mode": "all" }
                }])
            };
            offload.insert("backends".to_string(), backends);
        }
    }

    // Stamp a *literal* 17 (not CURRENT_SCHEMA_VERSION): the v17 → v18 step
    // below runs next in the same cascade pass and must still detect this file.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(17u8)),
    );
}

/// Is this a v1.17 file (schema_version == 17) that pre-dates the per-consumer
/// MCP access split (single `enabled` → `claude_access` + `offload_access`)?
fn looks_v1_17(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 17)
}

/// v1.17 → v1.18: split each MCP server's single `enabled` flag into
/// `claude_access` (new, opt-in → `false`) and `offload_access` (the legacy
/// behavior → the old `enabled` value, defaulting to `true` when absent).
fn migrate_v1_17_to_v1_18(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Object(offload)) = root.get_mut("offload") {
        if let Some(Value::Array(servers)) = offload.get_mut("mcp_servers") {
            for srv in servers.iter_mut() {
                if let Some(obj) = srv.as_object_mut() {
                    let enabled = obj
                        .remove("enabled")
                        .as_ref()
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    obj.entry("offload_access").or_insert(Value::Bool(enabled));
                    obj.entry("claude_access").or_insert(Value::Bool(false));
                }
            }
        }
    }

    // Stamp a *literal* 18 (not CURRENT_SCHEMA_VERSION): the v18 → v19 step
    // below runs next in the same cascade pass and must still detect this file.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(18u8)),
    );
}

/// Is this a v1.18 file (schema_version == 18) that pre-dates the V19
/// Aider → OpenCode replacement?
fn looks_v18(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 18)
}

fn migrate_v18_to_v19_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v18_to_v19(value)
}

/// The legacy V14 Aider reserved tab ids. BOTH collapse into the single V19
/// `opencode` tab (OpenCode has no cloud/local split — it switches providers
/// in-session), so a user with both aider tabs lands on one opencode tab.
const LEGACY_AIDER_LOCAL_TAB_ID: &str = "aider-local";

/// True for a legacy Aider reserved tab id (`aider` or `aider-local`).
fn is_legacy_aider_id(id: &str) -> bool {
    id == LEGACY_AIDER_TAB_ID || id == LEGACY_AIDER_LOCAL_TAB_ID
}

/// Remove later `"opencode"` string entries from a JSON array, keeping the
/// first occurrence and preserving the order of everything else. Used after
/// the id rewrite collapses both aider ids onto `opencode`, which can produce
/// duplicates in `tab_ids` / `enabled_ai_tabs`.
fn dedup_opencode_entries(arr: &mut Vec<Value>) {
    let mut seen = false;
    arr.retain(|v| {
        if v.as_str() == Some("opencode") {
            if seen {
                return false;
            }
            seen = true;
        }
        true
    });
}

/// v18 → v19: replace BOTH Aider reserved tabs with the single OpenCode tab.
///
/// 1. Drop the legacy `aider_local` provider settings group (OpenCode manages
///    its own providers — there is no `opencode_local`).
/// 2. Rewrite each reserved `aider` / `aider-local` tab in place to `opencode`:
///    id, command, name "OpenCode"; PRESERVE per-tab `env`; reset
///    `use_local_provider` to false and `args` to `[]` (dropping any stored
///    `--model`); enable TTS injection (OpenCode can speak); rewrite canonical
///    "Aider …" notification text. Then drop duplicate `opencode` tabs (a user
///    with both aider tabs keeps the first, i.e. the cloud one's config).
/// 3. Rewrite + dedupe layout-tree, layout-preset, and `session.active_tab_id`
///    references.
/// 4. Remap + dedupe `enabled_ai_tabs`.
/// 5. Default each MCP server's new `opencode_access` from `claude_access`.
///
/// Idempotent: a second pass finds `schema_version == 19` so `looks_v18` is
/// false, and no tab still carries an aider id.
fn migrate_v18_to_v19(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    // 1. Drop the legacy aider_local provider group (no opencode_local).
    root.remove("aider_local");

    // 2. Rewrite each reserved aider tab in place to `opencode`.
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            let Some(obj) = tab.as_object_mut() else {
                continue;
            };
            let old_id = obj
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !is_legacy_aider_id(&old_id) {
                continue;
            }
            let was_local = old_id == LEGACY_AIDER_LOCAL_TAB_ID;

            obj.insert("id".to_string(), Value::String("opencode".to_string()));
            obj.insert("command".to_string(), Value::String("opencode".to_string()));
            // Reset args (drops any stored `--model`) and use_local_provider
            // (OpenCode picks its own provider); env is preserved.
            obj.insert("args".to_string(), Value::Array(Vec::new()));
            obj.insert("use_local_provider".to_string(), Value::Bool(false));

            // Rewrite the canonical name only; leave a user-customized name.
            let old_name = if was_local { "Aider (local)" } else { "Aider" };
            if obj.get("name").and_then(Value::as_str) == Some(old_name) {
                obj.insert("name".to_string(), Value::String("OpenCode".to_string()));
            }

            // OpenCode can speak: enable TTS. Ensure the object exists.
            let tts_entry = obj
                .entry("tts_injection".to_string())
                .or_insert_with(|| json!({}));
            if let Some(tts) = tts_entry.as_object_mut() {
                tts.insert("enabled".to_string(), Value::Bool(true));
            }

            // Rewrite canonical "Aider …" / "Aider (local) …" notification text
            // → "OpenCode …".
            if let Some(notifs) = obj.get_mut("notifications").and_then(Value::as_object_mut) {
                let prefix = if was_local { "Aider (local)" } else { "Aider" };
                for field in ["idle", "awaiting_permission", "question", "error"] {
                    let suffix = match field {
                        "idle" => " is idle",
                        "awaiting_permission" => " is awaiting permission",
                        "question" => " has a question",
                        _ => " encountered an error",
                    };
                    let from_text = format!("{prefix}{suffix}");
                    if notifs.get(field).and_then(Value::as_str) == Some(from_text.as_str()) {
                        notifs.insert(
                            field.to_string(),
                            Value::String(format!("OpenCode{suffix}")),
                        );
                    }
                }
            }
        }

        // Drop duplicate `opencode` tabs (both aider tabs collapsed): keep the
        // first occurrence (canonical order puts the cloud `aider` tab first).
        let mut seen_opencode = false;
        tabs.retain(|t| {
            let is_opencode = t.get("id").and_then(Value::as_str) == Some("opencode");
            if is_opencode {
                if seen_opencode {
                    return false;
                }
                seen_opencode = true;
            }
            true
        });
    }

    // 3. Rewrite + dedupe layout-tree / preset / session id references.
    rewrite_opencode_tab_ids(root);

    // 4. Remap + dedupe enabled_ai_tabs.
    if let Some(enabled) = root
        .get_mut("enabled_ai_tabs")
        .and_then(Value::as_array_mut)
    {
        for entry in enabled.iter_mut() {
            if entry.as_str().is_some_and(is_legacy_aider_id) {
                *entry = Value::String("opencode".to_string());
            }
        }
        dedup_opencode_entries(enabled);
    }

    // 5. Default each MCP server's opencode_access from claude_access.
    if let Some(Value::Object(offload)) = root.get_mut("offload") {
        if let Some(Value::Array(servers)) = offload.get_mut("mcp_servers") {
            for srv in servers.iter_mut() {
                if let Some(obj) = srv.as_object_mut() {
                    let claude = obj
                        .get("claude_access")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    obj.entry("opencode_access").or_insert(Value::Bool(claude));
                }
            }
        }
    }

    // 6. Stamp this step's target version. The cascade continues to v20.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(19u8)),
    );
}

/// Is this a v19 file (schema_version == 19) that pre-dates the V20
/// fullscreen-only switch?
fn looks_v19(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 19)
}

fn migrate_v19_to_v20_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v19_to_v20(value)
}

/// V19 → V20: fullscreen-only AI tabs + out-of-band TTS.
///
/// - Strip any stored `--mini` from every tab's `args` (OpenCode now launches
///   in its native fullscreen TUI; the flag was the inline-mode forcing).
/// - Drop the retired `tts_all_output` field from every tab (the "speak all
///   output" scrape mode is gone — TTS is sourced out-of-band).
///
/// Everything else (including `copy_on_select` and the per-tab
/// `tts_injection`, now repurposed as the out-of-band speak gate) is preserved.
fn migrate_v19_to_v20(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            let Some(obj) = tab.as_object_mut() else {
                continue;
            };
            // Retire the speak-all field (serde would ignore it anyway, but
            // keep the on-disk file self-describing).
            obj.remove("tts_all_output");
            // Strip stored `--mini` from the tab's args.
            if let Some(Value::Array(args)) = obj.get_mut("args") {
                args.retain(|a| a.as_str() != Some("--mini"));
            }
        }
    }

    // Final cascade step ⇒ stamp CURRENT (20).
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(20u8)),
    );
}

fn looks_v20(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 20)
}

fn migrate_v20_to_v21_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v20_to_v21(value)
}

/// V20 → V21: V14 Phase F preview tab support.
///
/// No data transform needed — `preview_last_url` / `preview_allow_remote`
/// are new `Settings` root fields and `TabConfig::Preview` is a new
/// internally-tagged tab-config variant; all additive with
/// `#[serde(default)]`, so an existing v20 file (with zero Preview tabs and
/// no knowledge of the new root fields) round-trips unchanged. This step
/// exists purely to advance the version marker so the migration cascade's
/// fixpoint guard (`migrate_if_needed`) doesn't flag a v20 file as
/// under-migrated. Stamps a *literal* 21; the v21→v22 step runs next in the
/// same cascade pass and must still detect this file.
fn migrate_v20_to_v21(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(21u8)),
    );
}

/// The local-data tool set as it stood BEFORE schema v22 — the five names a
/// pre-v22 "web/docs only" cloud backend persisted as its `AllExcept`
/// exclusion (native `read_file`/`code_search`/`run_command` + the
/// `filesystem`/`git` MCP servers). Frozen literal on purpose: this is the
/// fingerprint the v21→v22 backfill matches on, so it must NOT track the live
/// `schema::LOCAL_DATA_TOOLS` (which has since grown `list_dir`).
const LOCAL_DATA_TOOLS_PRE_V22: &[&str] = &[
    "read_file",
    "code_search",
    "run_command",
    "filesystem",
    "git",
];

/// The local-data tool set as of schema v22 — the pre-v22 five plus `list_dir`
/// (added in the V21 milestone WITHOUT a migration; this step backfills it).
/// Frozen literal for the same reason as [`LOCAL_DATA_TOOLS_PRE_V22`]: any
/// future addition to `schema::LOCAL_DATA_TOOLS` needs its own migration step,
/// not a silent retroactive change to this one.
const LOCAL_DATA_TOOLS_V22: &[&str] = &[
    "read_file",
    "list_dir",
    "code_search",
    "run_command",
    "filesystem",
    "git",
];

fn looks_v21(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 21)
}

fn migrate_v21_to_v22_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v21_to_v22(value)
}

/// V21 → V22: backfill `list_dir` into any offload backend scoped "web/docs
/// only".
///
/// The V21 milestone added `list_dir` to `LOCAL_DATA_TOOLS` (the set a cloud
/// backend denies by default) but shipped no migration. A user who, on a
/// pre-V21 build, scoped a cloud backend to web/docs only persisted
/// `AllExcept { tools: [the old five] }` — so after upgrading, `list_dir`
/// (absent from that list) became *allowed*, silently handing a cloud backend
/// a local-data tool the user had explicitly opted out of. This step closes
/// that hole for every backend whose exclusion list is recognizably the
/// local-data preset.
///
/// Idempotent: a second pass finds `schema_version == 22` so `looks_v21` is
/// false, and the backfill only ever adds already-absent names.
fn migrate_v21_to_v22(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Object(offload)) = root.get_mut("offload") {
        if let Some(Value::Array(backends)) = offload.get_mut("backends") {
            for backend in backends.iter_mut() {
                backfill_local_data_scope(backend);
            }
        }
    }

    // Stamp a *literal* 22 (not CURRENT_SCHEMA_VERSION): the v22 → v23 step
    // gates on `schema_version == 22`, so this step must leave that concrete
    // value for the next detector to match.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(22u8)),
    );
}

fn looks_v22(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 22)
}

fn migrate_v22_to_v23_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v22_to_v23(value)
}

/// V22 → V23: drop the retired `code-quality` reserved tab.
///
/// The V25 milestone shipped Quality as a SECOND reserved tab next to Code
/// Audit; it's now a sub-tab inside the Code Audit view (Security | Quality),
/// so the separate tab entry must go. Without this step the old materialized
/// entry would deserialize as a plain closable Shell tab named "Code Quality"
/// (the `TabId::CodeQuality` variant no longer exists) with no view behind it.
/// Layout-tree references to the id don't need scrubbing here — the frontend's
/// `validateAndRepairLayout` drops any pane tab id absent from `tabs`.
///
/// Idempotent: a second pass finds `schema_version == 23` so `looks_v22` is
/// false, and the retain is a no-op once the entry is gone.
fn migrate_v22_to_v23(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Array(tabs)) = root.get_mut("tabs") {
        tabs.retain(|t| {
            t.get("id").and_then(Value::as_str)
                != Some(crate::settings::schema::CODE_QUALITY_TAB_ID)
        });
    }

    // Stamp a *literal* 23 (not CURRENT_SCHEMA_VERSION): the v23 → v24 step
    // runs next in the same cascade pass and gates on `schema_version == 23`,
    // so this step must leave that concrete value for the next detector.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(23u8)),
    );
}

fn looks_v23(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 23)
}

fn migrate_v23_to_v24_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v23_to_v24(value)
}

/// V23 → V24: pure version stamp for the V26 Code-Audit MCP-exposure flags.
///
/// V26 added three additive bools to `CodeAuditSettings`
/// (`expose_claude` / `expose_opencode` / `expose_offload`), all
/// `#[serde(default)]` → true. An existing v23 file that lacks them
/// round-trips with every flag defaulting on (see the schema test
/// `code_audit_v23_json_without_expose_flags_loads_true`), so no data
/// transform is needed. This step exists purely to advance the version marker
/// so the cascade's fixpoint guard (`migrate_if_needed`) doesn't flag a v23
/// file as under-migrated. Stamps a *literal* 24 (not
/// `CURRENT_SCHEMA_VERSION`): the v24 → v25 step runs next in the same
/// cascade pass and gates on `schema_version == 24`.
///
/// Idempotent: a second pass finds `schema_version == 24` so `looks_v23` is
/// false.
fn migrate_v23_to_v24(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(24u8)),
    );
}

fn looks_v24(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 24)
}

fn migrate_v24_to_v25_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v24_to_v25(value)
}

/// V24 → V25: drop the retired `offload-server` reserved tab.
///
/// The V8-03 milestone shipped the Offload Server dashboard as its own
/// reserved tab (materialized iff `offload.enabled`); it's now the "Offload
/// server" section inside the Tool Activity tab, so the separate tab entry
/// must go. Without this step the old materialized entry would deserialize as
/// a plain closable Shell tab named "Offload Server" (the
/// `TabId::OffloadServer` variant no longer exists) with no view behind it.
/// Layout-tree references to the id don't need scrubbing here — the frontend's
/// `validateAndRepairLayout` drops any pane tab id absent from `tabs`.
///
/// Idempotent: a second pass finds `schema_version == 25` so `looks_v24` is
/// false, and the retain is a no-op once the entry is gone.
fn migrate_v24_to_v25(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(Value::Array(tabs)) = root.get_mut("tabs") {
        tabs.retain(|t| {
            t.get("id").and_then(Value::as_str)
                != Some(crate::settings::schema::OFFLOAD_SERVER_TAB_ID)
        });
    }

    // Final cascade step ⇒ stamp CURRENT (25).
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(25u8)),
    );
}

/// Fail-closed backfill for one backend's `tool_scope`. If the scope is an
/// `AllExcept` whose exclusion list already denies the *entire* pre-v22
/// local-data preset (the "web/docs only" fingerprint), add any v22 local-data
/// tool missing from it. Any other shape is left untouched: `all`/`only`
/// scopes carry no privacy regression, and an `allexcept` list that denies
/// only a subset of the preset is a deliberate custom selection we must not
/// silently widen.
fn backfill_local_data_scope(backend: &mut Value) {
    let Some(scope) = backend.get_mut("tool_scope").and_then(Value::as_object_mut) else {
        return;
    };
    if scope.get("mode").and_then(Value::as_str) != Some("allexcept") {
        return;
    }
    let Some(Value::Array(tools)) = scope.get_mut("tools") else {
        return;
    };
    // Intent check: only widen when the exclusion already covers the whole
    // pre-v22 preset — that's the unambiguous "exclude local-data tools" signal.
    let covers_preset = LOCAL_DATA_TOOLS_PRE_V22
        .iter()
        .all(|name| tools.iter().any(|t| t.as_str() == Some(name)));
    if !covers_preset {
        return;
    }
    for name in LOCAL_DATA_TOOLS_V22 {
        if !tools.iter().any(|t| t.as_str() == Some(name)) {
            tools.push(Value::String((*name).to_string()));
        }
    }
}

/// Walk layout-tree-shaped JSON inside the settings root and rewrite any
/// `aider` / `aider-local` tab-id reference to `opencode`, de-duplicating the
/// `opencode` entries each `tab_ids` array then collapses to. Covers
/// `layout.tree`, every `layout_presets[].tree`, and `session.active_tab_id`.
fn rewrite_opencode_tab_ids(root: &mut Map<String, Value>) {
    fn rewrite_node(node: &mut Value) {
        let Some(obj) = node.as_object_mut() else {
            return;
        };
        if let Some(arr) = obj.get_mut("tab_ids").and_then(Value::as_array_mut) {
            for entry in arr.iter_mut() {
                if entry.as_str().is_some_and(is_legacy_aider_id) {
                    *entry = Value::String("opencode".to_string());
                }
            }
            dedup_opencode_entries(arr);
        }
        if obj
            .get("active_tab_id")
            .and_then(Value::as_str)
            .is_some_and(is_legacy_aider_id)
        {
            obj.insert(
                "active_tab_id".to_string(),
                Value::String("opencode".to_string()),
            );
        }
        if let Some(child) = obj.get_mut("first") {
            rewrite_node(child);
        }
        if let Some(child) = obj.get_mut("second") {
            rewrite_node(child);
        }
    }

    if let Some(layout) = root.get_mut("layout") {
        if let Some(tree) = layout.get_mut("tree") {
            rewrite_node(tree);
        }
    }
    if let Some(presets) = root.get_mut("layout_presets").and_then(Value::as_array_mut) {
        for preset in presets.iter_mut() {
            if let Some(obj) = preset.as_object_mut() {
                if let Some(tree) = obj.get_mut("tree") {
                    rewrite_node(tree);
                }
            }
        }
    }
    if let Some(session) = root.get_mut("session").and_then(Value::as_object_mut) {
        if session
            .get("active_tab_id")
            .and_then(Value::as_str)
            .is_some_and(is_legacy_aider_id)
        {
            session.insert(
                "active_tab_id".to_string(),
                Value::String("opencode".to_string()),
            );
        }
    }
}

// --- Backup helpers ---------------------------------------------------------

/// Write `<full-filename>.<from_version>.bak` next to the settings file. If
/// that name already exists (the user somehow rolled back and re-migrated),
/// append a unix timestamp to the suffix so the original backup survives.
/// Failure here aborts the migration — we never proceed without a
/// recoverable copy.
///
/// Backup filenames are built by *appending* to the full filename rather
/// than `with_extension`, which only knows the last dot — for a dotted name
/// like the legacy overlay `.cimp.custom.config.json`, `with_extension` would
/// consume `config` as the extension and produce `.cimp.custom.json.<ver>.bak`,
/// drifting the backup name away from the source's stem.
fn write_backup(path: &Path, from_version: &str, value: &Value) -> AppResult<()> {
    let primary = backup_path_for(path, &format!("{from_version}.bak"));
    let target = if primary.exists() {
        // Nanosecond resolution so two migrations within the same wall-clock
        // second don't collide (second-granularity used to let a rapid
        // relaunch / launch-loop overwrite the first timestamped backup). If
        // even the nanos name is taken — or the clock is unusable and we fall
        // back to 0 — probe with an incrementing counter for a free name so we
        // never clobber an existing backup.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut candidate = backup_path_for(path, &format!("{from_version}.bak.{nanos}"));
        let mut n = 0u32;
        while candidate.exists() && n < 10_000 {
            n += 1;
            candidate = backup_path_for(path, &format!("{from_version}.bak.{nanos}.{n}"));
        }
        candidate
    } else {
        primary
    };

    // If the probe exhausted without finding a free name, `target` still exists
    // — refuse rather than let `write_atomic`'s rename clobber an existing
    // backup, preserving the "never lose the original" guarantee. (Pathological:
    // requires ~10k identical-nanosecond collisions; aborts migration loudly,
    // matching the backup-write-failure contract.)
    if target.exists() {
        return Err(AppError::Settings(format!(
            "could not find a free backup name for {}; refusing to overwrite an \
             existing backup",
            target.display()
        )));
    }

    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| AppError::Settings(format!("backup serialize: {e}")))?;
    crate::settings::write_atomic(&target, &bytes).map_err(|e| {
        AppError::Settings(format!("backup write {} failed: {e}", target.display()))
    })?;
    tracing::info!(
        backup = %target.display(),
        from = from_version,
        "settings: pre-migration backup written"
    );
    Ok(())
}

/// Produce `<path-with-original-filename>.<suffix>`. Unlike
/// `Path::with_extension` this preserves the entire original filename
/// (including any embedded dots), so a backup of a dotted name like
/// `.cimp.custom.config.json` becomes `.cimp.custom.config.json.<suffix>`
/// rather than `.cimp.custom.json.<suffix>`.
fn backup_path_for(path: &Path, suffix: &str) -> PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut new_name = name.to_os_string();
            new_name.push(".");
            new_name.push(suffix);
            path.with_file_name(new_name)
        }
        None => PathBuf::from(format!("{}.{suffix}", path.display())),
    }
}

/// Move a corrupt settings file aside before resetting to defaults. Best-
/// effort: a failed rename falls back to copy+remove (cross-volume
/// rename fails on Windows when, e.g., the launch_cwd lives on a different
/// drive than the user temp). A total failure just logs and returns — the
/// caller still resets to defaults.
pub fn quarantine_corrupt_file(path: &Path) {
    if !path.exists() {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = backup_path_for(path, &format!("corrupted.{ts}.bak"));
    let renamed = fs::rename(path, &target).is_ok();
    let moved = if renamed {
        true
    } else if fs::copy(path, &target).is_ok() {
        let _ = fs::remove_file(path);
        true
    } else {
        false
    };
    if moved {
        tracing::warn!(
            quarantine = %target.display(),
            "settings: corrupt file moved aside; defaults will be written"
        );
    } else {
        tracing::warn!(
            path = %path.display(),
            target = %target.display(),
            "settings: could not quarantine corrupt file"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_default_shell() -> ShellSpec {
        ShellSpec {
            command: PathBuf::from("/bin/bash"),
            args: vec!["-i".to_string()],
        }
    }

    #[test]
    fn v17_to_v18_splits_mcp_enabled_into_two_flags() {
        let mut v = json!({
            "schema_version": 17,
            "offload": { "mcp_servers": [
                { "name": "ddg", "url": "http://x", "enabled": true },
                { "name": "git", "command": "uvx", "enabled": false },
                { "name": "fs", "command": "uvx" }, // missing enabled → defaults true
            ]}
        });
        migrate_v1_17_to_v1_18(&mut v);
        let s = v["offload"]["mcp_servers"].as_array().unwrap();
        // enabled:true → offload on, claude off; legacy key dropped.
        assert_eq!(s[0]["offload_access"], json!(true));
        assert_eq!(s[0]["claude_access"], json!(false));
        assert!(s[0].get("enabled").is_none());
        // enabled:false → offload off.
        assert_eq!(s[1]["offload_access"], json!(false));
        // absent → defaults to on (behavior-preserving for the common case).
        assert_eq!(s[2]["offload_access"], json!(true));
        // Stamps a literal 18 (no longer the final cascade step since V19).
        assert_eq!(v["schema_version"], json!(18));
    }

    #[test]
    fn v18_to_v19_collapses_both_aider_tabs_into_single_opencode() {
        let mut v = json!({
            "schema_version": 18,
            "aider_local": { "base_url": "http://host:9/v1", "auth_token": "tok", "model": "m" },
            "enabled_ai_tabs": ["claude", "aider", "aider-local"],
            "tabs": [
                { "id": "claude", "kind": "ai_tool", "command": "claude" },
                {
                    "id": "aider", "kind": "ai_tool", "command": "aider", "name": "Aider",
                    "args": ["--model", "gpt"], "use_local_provider": false,
                    "env": { "FOO": "bar" },
                    "notifications": { "idle": "Aider is idle" }
                },
                {
                    "id": "aider-local", "kind": "ai_tool", "command": "aider",
                    "name": "Aider (local)", "args": [], "use_local_provider": true,
                    "env": {}
                }
            ],
            "layout": { "tree": { "type": "pane", "id": "p1",
                "tab_ids": ["claude", "aider", "aider-local"], "active_tab_id": "aider" } },
            "session": { "active_tab_id": "aider-local" },
            "offload": { "mcp_servers": [
                { "name": "ddg", "url": "http://x", "claude_access": true, "offload_access": false },
                { "name": "git", "command": "uvx", "claude_access": false, "offload_access": true }
            ]}
        });
        migrate_v18_to_v19(&mut v);

        // Legacy provider group dropped; no opencode_local.
        assert!(v.get("aider_local").is_none());
        assert!(v.get("opencode_local").is_none());

        // Both aider tabs collapse into ONE opencode tab (the first/cloud one's
        // config wins): claude + opencode, length 2.
        let tabs = v["tabs"].as_array().unwrap();
        assert_eq!(
            tabs.len(),
            2,
            "the two aider tabs collapse into one opencode"
        );
        assert_eq!(tabs[0]["id"], "claude");
        assert_eq!(tabs[1]["id"], "opencode");
        assert_eq!(tabs[1]["command"], "opencode");
        assert_eq!(tabs[1]["name"], "OpenCode");
        assert_eq!(tabs[1]["args"], json!([])); // stored --model dropped
        assert_eq!(tabs[1]["env"]["FOO"], "bar"); // env preserved (from the cloud tab)
        assert_eq!(tabs[1]["use_local_provider"], json!(false)); // reset
        assert_eq!(tabs[1]["tts_injection"]["enabled"], json!(true));
        assert_eq!(tabs[1]["notifications"]["idle"], "OpenCode is idle");

        // Layout / session refs rewritten + deduped to a single opencode.
        let tree = &v["layout"]["tree"];
        assert_eq!(tree["tab_ids"], json!(["claude", "opencode"]));
        assert_eq!(tree["active_tab_id"], "opencode");
        assert_eq!(v["session"]["active_tab_id"], "opencode");

        // enabled_ai_tabs remapped + deduped.
        assert_eq!(v["enabled_ai_tabs"], json!(["claude", "opencode"]));

        // opencode_access defaults from claude_access.
        let s = v["offload"]["mcp_servers"].as_array().unwrap();
        assert_eq!(s[0]["opencode_access"], json!(true)); // claude_access true
        assert_eq!(s[1]["opencode_access"], json!(false)); // claude_access false

        // Stamped to the current version; not re-detected.
        assert_eq!(v["schema_version"], json!(19));
        assert!(!looks_v18(&v));
    }

    #[test]
    fn v18_to_v19_local_only_aider_becomes_opencode() {
        // A user who had ONLY the aider-local tab still lands on an opencode tab.
        let mut v = json!({
            "schema_version": 18,
            "enabled_ai_tabs": ["claude", "aider-local"],
            "tabs": [
                { "id": "claude", "kind": "ai_tool", "command": "claude" },
                { "id": "aider-local", "kind": "ai_tool", "command": "aider",
                  "name": "Aider (local)", "use_local_provider": true, "env": { "K": "v" } }
            ]
        });
        migrate_v18_to_v19(&mut v);
        let tabs = v["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1]["id"], "opencode");
        assert_eq!(tabs[1]["use_local_provider"], json!(false));
        assert_eq!(tabs[1]["env"]["K"], "v");
        assert_eq!(v["enabled_ai_tabs"], json!(["claude", "opencode"]));
        assert_eq!(v["schema_version"], json!(19));
    }

    #[test]
    fn looks_v19_detects_v19_and_not_others() {
        assert!(looks_v19(&json!({ "schema_version": 19 })));
        assert!(!looks_v19(&json!({ "schema_version": 18 })));
        assert!(!looks_v19(&json!({ "schema_version": 20 })));
        assert!(!looks_v19(&json!({})));
    }

    #[test]
    fn v19_to_v20_strips_mini_and_drops_tts_all_output() {
        let mut v = json!({
            "schema_version": 19,
            "tabs": [
                {
                    "id": "opencode", "kind": "ai_tool", "command": "opencode",
                    "args": ["--mini", "--model", "x"],
                    "tts_all_output": true,
                    "tts_injection": { "enabled": true, "instructions": "wrap" }
                },
                {
                    "id": "claude", "kind": "ai_tool", "command": "claude",
                    "args": ["--resume"], "tts_all_output": false
                }
            ],
            "behavior": { "copy_on_select": true }
        });
        migrate_v19_to_v20(&mut v);

        let tabs = v["tabs"].as_array().unwrap();
        // --mini stripped, other args intact.
        assert_eq!(tabs[0]["args"], json!(["--model", "x"]));
        assert_eq!(tabs[1]["args"], json!(["--resume"]));
        // tts_all_output removed from every tab.
        assert!(tabs[0].get("tts_all_output").is_none());
        assert!(tabs[1].get("tts_all_output").is_none());
        // Repurposed gate + unrelated settings preserved.
        assert_eq!(tabs[0]["tts_injection"]["enabled"], json!(true));
        assert_eq!(v["behavior"]["copy_on_select"], json!(true));
        // Stamped CURRENT.
        assert_eq!(v["schema_version"], json!(20));
        assert!(!looks_v19(&v));
    }

    #[test]
    fn v19_to_v20_is_idempotent() {
        let mut v = json!({
            "schema_version": 19,
            "tabs": [{ "id": "opencode", "kind": "ai_tool", "command": "opencode", "args": [] }]
        });
        migrate_v19_to_v20(&mut v);
        let once = v.clone();
        // A second pass changes nothing material (no --mini, no tts_all_output).
        migrate_v19_to_v20(&mut v);
        assert_eq!(v, once);
        assert_eq!(v["schema_version"], json!(20));
    }

    #[test]
    fn looks_v20_detects_v20_and_not_others() {
        assert!(looks_v20(&json!({ "schema_version": 20 })));
        assert!(!looks_v20(&json!({ "schema_version": 19 })));
        assert!(!looks_v20(&json!({ "schema_version": 21 })));
        assert!(!looks_v20(&json!({})));
    }

    /// V14 Phase F: a v20 file — including one with existing (non-Preview)
    /// tabs — round-trips through the v20→v21 step with everything but the
    /// version marker untouched; the new root fields simply aren't present
    /// (they'll deserialize to their `#[serde(default)]` values).
    #[test]
    fn v20_to_v21_is_additive_only() {
        let mut v = json!({
            "schema_version": 20,
            "tabs": [{ "id": "claude", "kind": "ai_tool", "command": "claude" }],
            "behavior": { "copy_on_select": true }
        });
        let before_tabs = v["tabs"].clone();
        migrate_v20_to_v21(&mut v);

        assert_eq!(v["schema_version"], json!(21));
        assert_eq!(v["tabs"], before_tabs);
        assert_eq!(v["behavior"]["copy_on_select"], json!(true));
        assert!(!looks_v20(&v));
        assert!(v.get("preview_last_url").is_none());
        assert!(v.get("preview_allow_remote").is_none());
    }

    #[test]
    fn v20_to_v21_is_idempotent() {
        let mut v = json!({ "schema_version": 20, "tabs": [] });
        migrate_v20_to_v21(&mut v);
        let once = v.clone();
        migrate_v20_to_v21(&mut v);
        assert_eq!(v, once);
    }

    // --- v21 → v22 (list_dir scope backfill) --------------------------------

    #[test]
    fn looks_v21_detects_v21_and_not_others() {
        assert!(looks_v21(&json!({ "schema_version": 21 })));
        assert!(!looks_v21(&json!({ "schema_version": 20 })));
        assert!(!looks_v21(&json!({ "schema_version": 22 })));
        assert!(!looks_v21(&json!({})));
    }

    /// The security fix: a backend scoped to the pre-v22 local-data preset (the
    /// old five names) gains `list_dir` so V21's new local-data tool stays
    /// denied on a cloud backend the user opted out of.
    #[test]
    fn v21_to_v22_backfills_list_dir_into_web_scoped_backend() {
        let mut v = json!({
            "schema_version": 21,
            "offload": {
                "backends": [{
                    "name": "cloud",
                    "kind": { "type": "remote", "is_cloud": true },
                    "tool_scope": {
                        "mode": "allexcept",
                        "tools": ["read_file", "code_search", "run_command", "filesystem", "git"]
                    }
                }]
            }
        });
        migrate_v21_to_v22(&mut v);

        assert_eq!(v["schema_version"], json!(22));
        let tools = v.pointer("/offload/backends/0/tool_scope/tools").unwrap();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"list_dir"),
            "list_dir must be backfilled: {names:?}"
        );
        // Every pre-existing exclusion is preserved.
        for name in LOCAL_DATA_TOOLS_PRE_V22 {
            assert!(
                names.contains(name),
                "dropped pre-existing exclusion {name}"
            );
        }
        assert!(!looks_v21(&v));
    }

    /// A custom `AllExcept` list that is NOT the local-data preset (only denies
    /// a subset) is a deliberate user selection and must stay untouched.
    #[test]
    fn v21_to_v22_leaves_non_preset_scopes_untouched() {
        let mut v = json!({
            "schema_version": 21,
            "offload": {
                "backends": [
                    {
                        "name": "partial-custom",
                        // Denies only `git` — not the full preset ⇒ leave alone.
                        "tool_scope": { "mode": "allexcept", "tools": ["git"] }
                    },
                    {
                        "name": "whitelist",
                        // `only` scope carries no privacy regression ⇒ leave alone.
                        "tool_scope": { "mode": "only", "tools": ["duckduckgo"] }
                    },
                    {
                        "name": "trusted-lan",
                        "tool_scope": { "mode": "all" }
                    }
                ]
            }
        });
        migrate_v21_to_v22(&mut v);

        assert_eq!(
            v.pointer("/offload/backends/0/tool_scope/tools").unwrap(),
            &json!(["git"]),
            "partial custom allexcept list must not be widened"
        );
        assert_eq!(
            v.pointer("/offload/backends/1/tool_scope/tools").unwrap(),
            &json!(["duckduckgo"]),
            "only-scope must not be touched"
        );
        assert_eq!(
            v.pointer("/offload/backends/2/tool_scope/mode").unwrap(),
            "all"
        );
    }

    #[test]
    fn v21_to_v22_is_idempotent() {
        let mut v = json!({
            "schema_version": 21,
            "offload": {
                "backends": [{
                    "tool_scope": {
                        "mode": "allexcept",
                        "tools": ["read_file", "code_search", "run_command", "filesystem", "git"]
                    }
                }]
            }
        });
        migrate_v21_to_v22(&mut v);
        let once = v.clone();
        migrate_v21_to_v22(&mut v);
        assert_eq!(v, once, "second pass must not add duplicate list_dir");
    }

    /// A v21 file with no offload block (feature never configured) just gets
    /// its version marker advanced.
    #[test]
    fn v21_to_v22_no_offload_only_stamps_version() {
        let mut v = json!({ "schema_version": 21, "tabs": [] });
        migrate_v21_to_v22(&mut v);
        assert_eq!(v["schema_version"], json!(22));
    }

    /// v22 → v23: the retired `code-quality` reserved tab entry is dropped;
    /// every other tab (including Code Audit) survives untouched.
    #[test]
    fn v22_to_v23_drops_retired_code_quality_tab() {
        let mut v = json!({
            "schema_version": 22,
            "tabs": [
                { "kind": "ai_tool", "id": "claude", "builtin": true },
                { "kind": "shell", "id": "code-audit", "builtin": true, "name": "Code Audit" },
                { "kind": "shell", "id": "code-quality", "builtin": true, "name": "Code Quality" },
                { "kind": "shell", "id": "shell-default-1", "builtin": false }
            ]
        });
        migrate_v22_to_v23(&mut v);
        assert_eq!(v["schema_version"], json!(23));
        let ids: Vec<&str> = v["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["claude", "code-audit", "shell-default-1"]);
    }

    /// A v22 file that never materialized the audit tabs (feature off) just
    /// gets its version marker advanced; running the step twice is a no-op.
    #[test]
    fn v22_to_v23_is_idempotent_and_stamps_version() {
        let mut v = json!({ "schema_version": 22, "tabs": [] });
        migrate_v22_to_v23(&mut v);
        assert_eq!(v["schema_version"], json!(23));
        let once = v.clone();
        migrate_v22_to_v23(&mut v);
        assert_eq!(v, once);
    }

    #[test]
    fn looks_v23_detects_v23_and_not_others() {
        assert!(looks_v23(&json!({ "schema_version": 23 })));
        assert!(!looks_v23(&json!({ "schema_version": 22 })));
        assert!(!looks_v23(&json!({ "schema_version": 24 })));
        assert!(!looks_v23(&json!({})));
    }

    /// V26: a v23 file — including one carrying a `code_audit` block that
    /// predates the three MCP-exposure flags — round-trips through the v23→v24
    /// step with everything but the version marker untouched; the new
    /// `expose_*` fields simply aren't present on disk (they deserialize to
    /// their `#[serde(default)]` = true values). Running the step twice is a
    /// no-op.
    #[test]
    fn v23_to_v24_only_stamps_version_and_is_idempotent() {
        let mut v = json!({
            "schema_version": 23,
            "code_audit": { "enabled": true, "tools": [] }
        });
        migrate_v23_to_v24(&mut v);
        assert_eq!(v["schema_version"], json!(24));
        // The code_audit block is carried through verbatim — no expose_* keys
        // are injected on disk; serde defaults fill them on load.
        assert_eq!(v["code_audit"], json!({ "enabled": true, "tools": [] }));
        let once = v.clone();
        migrate_v23_to_v24(&mut v);
        assert_eq!(v, once);
    }

    #[test]
    fn looks_v24_detects_v24_and_not_others() {
        assert!(looks_v24(&json!({ "schema_version": 24 })));
        assert!(!looks_v24(&json!({ "schema_version": 23 })));
        assert!(!looks_v24(&json!({ "schema_version": 25 })));
        assert!(!looks_v24(&json!({})));
    }

    /// v24 → v25: the retired `offload-server` reserved tab entry is dropped;
    /// every other tab survives untouched.
    #[test]
    fn v24_to_v25_drops_retired_offload_server_tab() {
        let mut v = json!({
            "schema_version": 24,
            "tabs": [
                { "kind": "ai_tool", "id": "claude", "builtin": true },
                { "kind": "shell", "id": "offload-server", "builtin": true, "name": "Offload Server" },
                { "kind": "shell", "id": "tool-activity", "builtin": true, "name": "Tool Activity" },
                { "kind": "shell", "id": "shell-default-1", "builtin": false }
            ]
        });
        migrate_v24_to_v25(&mut v);
        assert_eq!(v["schema_version"], json!(25));
        let ids: Vec<&str> = v["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["claude", "tool-activity", "shell-default-1"]);
    }

    /// A v24 file that never materialized the Offload Server tab (offload off)
    /// just gets its version marker advanced; running the step twice is a
    /// no-op.
    #[test]
    fn v24_to_v25_is_idempotent_and_stamps_version() {
        let mut v = json!({ "schema_version": 24, "tabs": [] });
        migrate_v24_to_v25(&mut v);
        assert_eq!(v["schema_version"], json!(25));
        let once = v.clone();
        migrate_v24_to_v25(&mut v);
        assert_eq!(v, once);
    }

    /// Altitude tripwire: if `schema::LOCAL_DATA_TOOLS` ever gains a member,
    /// migrating a settings file whose cloud backend carries the historical
    /// five-item "web/docs only" exclusion must still yield an exclusion list
    /// that denies EVERY current local-data tool. This fails the moment a new
    /// local-data tool is added without a matching migration step to backfill
    /// it (exactly the V21 `list_dir` regression this fix addresses) — forcing
    /// the author to add the migration rather than silently re-opening the hole.
    #[test]
    fn local_data_tools_growth_requires_a_backfilling_migration() {
        let shell = fake_default_shell();
        // The pristine pre-v22 web-scope fingerprint at the schema version it
        // was persisted under.
        let mut v = json!({
            "schema_version": 21,
            "offload": {
                "backends": [{
                    "name": "cloud",
                    "tool_scope": {
                        "mode": "allexcept",
                        "tools": ["read_file", "code_search", "run_command", "filesystem", "git"]
                    }
                }]
            }
        });
        // Drive the whole cascade the way `migrate_if_needed` does (minus the
        // backup write), so any future schema-bumping step also runs.
        for step in MIGRATION_STEPS {
            if (step.detect)(&v) {
                (step.transform)(&mut v, &shell);
            }
        }

        let tools: Vec<String> = v
            .pointer("/offload/backends/0/tool_scope/tools")
            .and_then(Value::as_array)
            .expect("web-scoped backend keeps an exclusion list")
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();

        for tool in crate::settings::schema::LOCAL_DATA_TOOLS {
            assert!(
                tools.contains(&tool.to_string()),
                "LOCAL_DATA_TOOLS member `{tool}` is not denied after migrating a pre-existing \
                 web/docs-only cloud backend — a new local-data tool was added to \
                 schema::LOCAL_DATA_TOOLS without a migration step backfilling it into existing \
                 `AllExcept` scopes. Add a v{}→v{} (or later) step that extends the local-data \
                 preset, mirroring migrate_v21_to_v22.",
                crate::settings::schema::CURRENT_SCHEMA_VERSION - 1,
                crate::settings::schema::CURRENT_SCHEMA_VERSION,
            );
        }
    }

    #[test]
    fn v1_1_to_v1_2_collapses_extra_flags_into_args() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": {
                    "claude": {
                        "command": "claude",
                        "extra_cli_flags": ["--foo", "--bar"],
                        "tts_injection": { "enabled": true, "instructions": "" },
                        "notifications": {
                            "idle": "Claude is idle",
                            "awaiting_permission": "Claude is awaiting permission",
                            "error": "Claude encountered an error"
                        },
                        "first_launch_notice_dismissed": true
                    },
                    "aider": {
                        "command": "aider",
                        "extra_cli_flags": [],
                        "tts_injection": { "enabled": false, "instructions": "" },
                        "notifications": {
                            "idle": "Aider is idle",
                            "awaiting_permission": "Aider is awaiting permission",
                            "error": "Aider encountered an error"
                        },
                        "first_launch_notice_dismissed": false
                    }
                },
                "_shell_1_tmp": {
                    "name": "Shell 1",
                    "notifications": {
                        "error": "Shell encountered an error",
                        "exited": "Shell exited (code {code})"
                    }
                }
            }"#,
        )
        .unwrap();

        let shell = fake_default_shell();
        migrate_v1_1_to_v1_2(&mut v, &shell);

        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        assert_eq!(tabs.len(), 3);

        let claude = &tabs[0];
        assert_eq!(claude.get("kind").unwrap(), "ai_tool");
        assert_eq!(claude.get("id").unwrap(), CLAUDE_TAB_ID);
        let args = claude.get("args").unwrap().as_array().unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--foo");
        assert_eq!(args[1], "--bar");
        assert_eq!(claude.get("builtin").unwrap(), &Value::Bool(true));

        let aider = &tabs[1];
        assert_eq!(aider.get("id").unwrap(), LEGACY_AIDER_TAB_ID);

        let shell_entry = &tabs[2];
        assert_eq!(shell_entry.get("kind").unwrap(), "shell");
        assert_eq!(shell_entry.get("id").unwrap(), SHELL_DEFAULT_TAB_ID);
        assert_eq!(shell_entry.get("name").unwrap(), "Shell 1");

        assert!(v.get("_shell_1_tmp").is_none());
    }

    #[test]
    fn v1_2_file_is_not_re_detected() {
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" }
                ]
            }"#,
        )
        .unwrap();
        assert!(!looks_v1(&v));
        assert!(!looks_v1_1(&v));
    }

    #[test]
    fn modern_file_missing_layout_is_not_misdetected_as_v1_2() {
        // Regression: a current-schema file (schema_version present) that
        // happens to lack the `layout` key must NOT trip the pre-v1.10
        // archaeology detectors, which would otherwise drag it back through
        // the whole cascade and write a fresh `.v1.2.bak` on every launch.
        // Tracks CURRENT_SCHEMA_VERSION so a schema bump can't silently turn
        // this fixture into a stale (migration-needing) file.
        let v = json!({
            "schema_version": crate::settings::schema::CURRENT_SCHEMA_VERSION,
            "tabs": [
                { "kind": "ai_tool", "id": "claude", "name": "Claude" }
            ]
        });
        assert!(
            !looks_v1_2(&v),
            "schema_version-bearing file matched looks_v1_2"
        );
        assert!(!looks_v1_3(&v));
        assert!(!looks_v1_4(&v));
        assert!(!looks_v1_5(&v));
        assert!(!looks_v1_6(&v));
        assert!(!looks_v1_7(&v));
        assert!(
            detect_entry_version(&v).is_none(),
            "modern file should need no migration"
        );
    }

    #[test]
    fn genuine_v1_2_file_still_detected_without_schema_version() {
        // The guard must not break real pre-v1.10 files: a v1.2-shape file
        // (tabs array, no layout, no schema_version) must still enter the
        // cascade at v1.2.
        let v: Value =
            serde_json::from_str(r#"{ "tabs": [ { "kind": "ai_tool", "id": "claude" } ] }"#)
                .unwrap();
        assert!(looks_v1_2(&v));
        assert_eq!(detect_entry_version(&v), Some("v1.2"));
    }

    #[test]
    fn v1_1_with_missing_aider_uses_default() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": {
                    "claude": {
                        "command": "claude",
                        "extra_cli_flags": []
                    }
                }
            }"#,
        )
        .unwrap();

        let shell = fake_default_shell();
        migrate_v1_1_to_v1_2(&mut v, &shell);

        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        assert_eq!(tabs.len(), 3);
        let aider = &tabs[1];
        assert_eq!(aider.get("id").unwrap(), LEGACY_AIDER_TAB_ID);
        assert_eq!(aider.get("command").unwrap(), "aider");
    }

    #[test]
    fn v1_to_v1_2_carries_extra_cli_args_to_claude_args() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "claude_code": { "extra_cli_args": ["--verbose"], "claude_md_path": null }
            }"#,
        )
        .unwrap();

        assert!(looks_v1(&v));
        let shell = fake_default_shell();
        migrate_v1_to_v1_2(&mut v, &shell);

        assert!(v.get("claude_code").is_none());
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        assert_eq!(tabs.len(), 3);
        let claude = &tabs[0];
        let args = claude.get("args").unwrap().as_array().unwrap();
        assert_eq!(args, &vec![Value::String("--verbose".to_string())]);

        // After migration, the v1.1 detector should not re-fire on this
        // value (tabs is now an array).
        assert!(!looks_v1_1(&v));
    }

    #[test]
    fn v1_2_to_v1_3_builds_single_pane_with_all_tabs() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" },
                    { "kind": "ai_tool", "id": "aider", "name": "Aider" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ],
                "session": { "active_tab_id": "aider" }
            }"#,
        )
        .unwrap();

        assert!(looks_v1_2(&v));
        migrate_v1_2_to_v1_3(&mut v);

        let layout = v.get("layout").expect("layout inserted");
        let tree = layout.get("tree").expect("tree present");
        assert_eq!(tree.get("type").unwrap(), "pane");
        let tab_ids = tree.get("tab_ids").unwrap().as_array().unwrap();
        assert_eq!(tab_ids.len(), 3);
        assert_eq!(tab_ids[0], "claude");
        assert_eq!(tab_ids[1], "aider");
        assert_eq!(tab_ids[2], "shell-default-1");
        assert_eq!(tree.get("active_tab_id").unwrap(), "aider");

        let pane_id = tree.get("id").unwrap().as_str().unwrap();
        let focused = layout.get("focused_pane_id").unwrap().as_str().unwrap();
        assert_eq!(focused, pane_id);

        // session.active_tab_id is dropped (the layout owns it now).
        assert!(v
            .get("session")
            .and_then(|s| s.get("active_tab_id"))
            .is_none());

        // layout_presets initialised to empty array.
        assert_eq!(
            v.get("layout_presets").unwrap().as_array().unwrap().len(),
            0
        );

        // Re-running migration is a no-op (layout key now present).
        assert!(!looks_v1_2(&v));
    }

    #[test]
    fn v1_2_to_v1_3_falls_back_to_first_tab_when_no_session() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ]
            }"#,
        )
        .unwrap();

        migrate_v1_2_to_v1_3(&mut v);
        let active = v
            .get("layout")
            .and_then(|l| l.get("tree"))
            .and_then(|t| t.get("active_tab_id"))
            .unwrap();
        assert_eq!(active, "claude");
    }

    #[test]
    fn v1_3_file_is_not_re_detected() {
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "name": "Claude" }],
                "layout": {
                    "tree": { "type": "pane", "id": "pane-1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                    "focused_pane_id": "pane-1"
                }
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_2(&v));
    }

    #[test]
    fn v1_3_with_null_layout_field_is_not_re_migrated() {
        // Fresh-install file written by v1.3: layout serialized as null
        // (Option::None → null in serde_json). The detector must skip
        // this so we don't pile up `.v1.2.bak.<ts>` files on every
        // launch of an app that hasn't yet been multi-paned.
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "name": "Claude" }],
                "layout": null,
                "layout_presets": []
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_2(&v));
    }

    #[test]
    fn v1_3_to_v1_4_adds_terminal_group_and_stamps_overrides() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "display": { "terminal_font_size": 14, "theme": "dark" },
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ],
                "layout": {
                    "tree": { "type": "pane", "id": "pane-1", "tab_ids": ["claude", "shell-default-1"], "active_tab_id": "claude" },
                    "focused_pane_id": "pane-1"
                }
            }"#,
        )
        .unwrap();

        assert!(looks_v1_3(&v));
        migrate_v1_3_to_v1_4(&mut v);

        // terminal.theme.name == "Default"
        let term_theme = v
            .get("terminal")
            .and_then(|t| t.get("theme"))
            .expect("terminal.theme inserted");
        assert_eq!(term_theme.get("name").unwrap(), "Default");
        assert!(term_theme.get("custom").unwrap().is_null());

        // display.theme dropped
        assert!(v.get("display").and_then(|d| d.get("theme")).is_none());

        // Every tab has theme_override: null
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        for tab in tabs {
            let key = tab
                .as_object()
                .and_then(|o| o.get("theme_override"))
                .expect("theme_override stamped");
            assert!(key.is_null());
        }

        // Re-detection is false after migration.
        assert!(!looks_v1_3(&v));
    }

    #[test]
    fn v1_4_file_is_not_re_detected() {
        // Fresh-install v1.4 file: `terminal` key is present.
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "theme_override": null }],
                "layout": null,
                "terminal": { "theme": { "name": "Default", "custom": null } }
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_3(&v));
    }

    #[test]
    fn v1_2_cascades_through_v1_3_and_v1_4() {
        // A v1.2 file (tabs array, no layout) coming in cold should
        // exit migration as a v1.4 file: layout populated, terminal
        // group populated, theme_override stamped on every tab.
        let mut v: Value = serde_json::from_str(
            r#"{
                "display": { "theme": "dark" },
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ]
            }"#,
        )
        .unwrap();

        // Simulate the dispatcher's two passes.
        assert!(looks_v1_2(&v));
        migrate_v1_2_to_v1_3(&mut v);
        assert!(looks_v1_3(&v));
        migrate_v1_3_to_v1_4(&mut v);

        // Final state: layout from v1.2→v1.3, terminal from v1.3→v1.4.
        assert!(v.get("layout").is_some());
        assert!(v.get("terminal").is_some());
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        for tab in tabs {
            assert!(tab.get("theme_override").unwrap().is_null());
        }
    }

    #[test]
    fn v1_4_to_v1_5_adds_background_group_and_stamps_overrides() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude", "theme_override": null },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1", "theme_override": null }
                ],
                "layout": {
                    "tree": { "type": "pane", "id": "pane-1", "tab_ids": ["claude", "shell-default-1"], "active_tab_id": "claude" },
                    "focused_pane_id": "pane-1"
                },
                "terminal": {
                    "theme": { "name": "Default", "custom": null }
                }
            }"#,
        )
        .unwrap();

        assert!(looks_v1_4(&v));
        migrate_v1_4_to_v1_5(&mut v);

        // terminal.background populated with milestone-doc defaults.
        let bg = v
            .get("terminal")
            .and_then(|t| t.get("background"))
            .expect("terminal.background inserted");
        assert!(bg.get("image").unwrap().is_null());
        assert!(bg.get("color").unwrap().is_null());
        assert_eq!(bg.get("opacity").unwrap().as_f64().unwrap(), 0.4);
        assert_eq!(bg.get("blur").unwrap().as_u64().unwrap(), 0);
        assert_eq!(bg.get("size").unwrap(), "cover");
        assert_eq!(bg.get("position").unwrap(), "center");

        // Every tab has background_override: null.
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        for tab in tabs {
            let key = tab
                .as_object()
                .and_then(|o| o.get("background_override"))
                .expect("background_override stamped");
            assert!(key.is_null());
        }

        // Re-detection is false after migration.
        assert!(!looks_v1_4(&v));
    }

    #[test]
    fn v1_5_file_is_not_re_detected() {
        // Fresh-install v1.5 file: `terminal.background` is present.
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "theme_override": null, "background_override": null }],
                "layout": null,
                "terminal": {
                    "theme": { "name": "Default", "custom": null },
                    "background": { "image": null, "color": null, "opacity": 0.4, "blur": 0, "size": "cover", "position": "center" }
                }
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_4(&v));
    }

    #[test]
    fn v1_5_to_v1_6_adds_empty_presets_array() {
        // V1.4-04 B: a fresh-install v1.5 file (terminal.background
        // present, no presets field) should be detected as v1.5 and
        // gain `terminal.background.presets: []`.
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "theme_override": null, "background_override": null }],
                "layout": null,
                "terminal": {
                    "theme": { "name": "Default", "custom": null },
                    "background": { "image": null, "color": null, "opacity": 0.4, "blur": 0, "size": "cover", "position": "center" }
                }
            }"#,
        )
        .unwrap();

        assert!(looks_v1_5(&v));
        migrate_v1_5_to_v1_6(&mut v);
        let bg = v
            .get("terminal")
            .and_then(|t| t.get("background"))
            .expect("terminal.background present");
        assert!(bg.get("presets").unwrap().is_array());
        assert_eq!(bg.get("presets").unwrap().as_array().unwrap().len(), 0);
        assert!(!looks_v1_5(&v));
    }

    #[test]
    fn v1_6_file_is_not_re_detected() {
        // Fresh-install v1.6 file: presets is present.
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "theme_override": null, "background_override": null }],
                "layout": null,
                "terminal": {
                    "theme": { "name": "Default", "custom": null },
                    "background": { "image": null, "color": null, "opacity": 0.4, "blur": 0, "size": "cover", "position": "center", "presets": [] }
                }
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_5(&v));
    }

    #[test]
    fn v1_2_cascades_through_v1_3_v1_4_v1_5_and_v1_6() {
        // V1.4-04 B: extends `v1_2_cascades_through_v1_3_v1_4_and_v1_5`
        // with one more step. A v1.2 file should land at v1.6 after
        // every transform fires in order.
        let mut v: Value = serde_json::from_str(
            r#"{
                "display": { "theme": "dark" },
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ]
            }"#,
        )
        .unwrap();

        assert!(looks_v1_2(&v));
        migrate_v1_2_to_v1_3(&mut v);
        assert!(looks_v1_3(&v));
        migrate_v1_3_to_v1_4(&mut v);
        assert!(looks_v1_4(&v));
        migrate_v1_4_to_v1_5(&mut v);
        assert!(looks_v1_5(&v));
        migrate_v1_5_to_v1_6(&mut v);

        let bg = v
            .get("terminal")
            .and_then(|t| t.get("background"))
            .expect("terminal.background present after cascade");
        assert!(bg.get("presets").unwrap().is_array());
        assert!(!looks_v1_5(&v));
    }

    #[test]
    fn v1_6_to_v1_7_adds_scrollback_and_preview_flag() {
        // V1.4-04 D: a v1.6 file (presets present, no scrollback group)
        // should be detected as v1.6 and gain `terminal.scrollback`
        // defaults plus the explicit `preview_category_flips` field.
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "theme_override": null, "background_override": null }],
                "layout": null,
                "terminal": {
                    "theme": { "name": "Default", "custom": null },
                    "background": { "image": null, "color": null, "opacity": 0.4, "blur": 0, "size": "cover", "position": "center", "presets": [] }
                }
            }"#,
        )
        .unwrap();

        assert!(looks_v1_6(&v));
        migrate_v1_6_to_v1_7(&mut v);

        let terminal = v.get("terminal").expect("terminal present");
        let scrollback = terminal.get("scrollback").expect("scrollback added");
        assert_eq!(scrollback.get("ring_bytes").unwrap(), 262144);
        assert_eq!(scrollback.get("persist").unwrap(), true);
        assert_eq!(scrollback.get("restore_on_launch").unwrap(), true);

        let bg = terminal.get("background").unwrap();
        assert_eq!(bg.get("preview_category_flips").unwrap(), true);

        assert!(!looks_v1_6(&v));
    }

    #[test]
    fn v1_7_file_is_not_re_detected() {
        // Fresh-install v1.7 file: scrollback group present.
        let v: Value = serde_json::from_str(
            r#"{
                "tabs": [{ "kind": "ai_tool", "id": "claude", "theme_override": null, "background_override": null }],
                "layout": null,
                "terminal": {
                    "theme": { "name": "Default", "custom": null },
                    "background": { "image": null, "color": null, "opacity": 0.4, "blur": 0, "size": "cover", "position": "center", "presets": [], "preview_category_flips": true },
                    "scrollback": { "ring_bytes": 262144, "persist": true, "restore_on_launch": true }
                }
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_5(&v));
        assert!(!looks_v1_6(&v));
    }

    #[test]
    fn v1_6_to_v1_7_preserves_existing_preview_flag() {
        // If a hand-edited v1.6 file already has
        // preview_category_flips set (e.g., user set it to false),
        // migration must not overwrite it.
        let mut v: Value = serde_json::from_str(
            r#"{
                "tabs": [],
                "layout": null,
                "terminal": {
                    "theme": { "name": "Default", "custom": null },
                    "background": { "image": null, "color": null, "opacity": 0.4, "blur": 0, "size": "cover", "position": "center", "presets": [], "preview_category_flips": false }
                }
            }"#,
        )
        .unwrap();
        assert!(looks_v1_6(&v));
        migrate_v1_6_to_v1_7(&mut v);
        let bg = v.get("terminal").and_then(|t| t.get("background")).unwrap();
        assert_eq!(bg.get("preview_category_flips").unwrap(), false);
    }

    #[test]
    fn v1_2_cascades_through_v1_3_v1_4_and_v1_5() {
        // A v1.2 file (tabs array, no layout) coming in cold should
        // exit migration as a v1.5 file: layout populated, terminal
        // group populated with theme + background, theme_override and
        // background_override stamped on every tab.
        let mut v: Value = serde_json::from_str(
            r#"{
                "display": { "theme": "dark" },
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ]
            }"#,
        )
        .unwrap();

        assert!(looks_v1_2(&v));
        migrate_v1_2_to_v1_3(&mut v);
        assert!(looks_v1_3(&v));
        migrate_v1_3_to_v1_4(&mut v);
        assert!(looks_v1_4(&v));
        migrate_v1_4_to_v1_5(&mut v);

        // Final state: layout, terminal.theme, terminal.background, both
        // override fields on each tab.
        assert!(v.get("layout").is_some());
        let terminal = v.get("terminal").expect("terminal present");
        assert!(terminal.get("theme").is_some());
        assert!(terminal.get("background").is_some());
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        for tab in tabs {
            assert!(tab.get("theme_override").unwrap().is_null());
            assert!(tab.get("background_override").unwrap().is_null());
        }
        assert!(!looks_v1_4(&v));
    }

    #[test]
    fn shell_interim_with_custom_name_preserves_name() {
        let interim = json!({
            "name": "My Shell",
            "notifications": {
                "error": "boom",
                "exited": "shell exited code {code}"
            }
        });
        let shell = fake_default_shell();
        let entry = transform_shell_1_from_interim(&interim, &shell);
        assert_eq!(entry.get("name").unwrap(), "My Shell");
        assert_eq!(
            entry.get("notifications").unwrap().get("error").unwrap(),
            "boom"
        );
    }

    // --- v1.7 → v1.8 (V1.4-07) ---

    /// Helper: a v1.7-shape settings file with a stock claude tab and a
    /// stock aider tab plus a layout that puts both side-by-side. Used
    /// as the starting state for several v1.7 → v1.8 tests.
    fn v1_7_with_aider_tab() -> Value {
        json!({
            "tabs": [
                {
                    "kind": "ai_tool",
                    "id": "claude",
                    "ai_tool_kind": "claude_code",
                    "builtin": true,
                    "name": "Claude",
                    "command": "claude",
                    "args": [],
                    "cwd": null,
                    "env": {},
                    "tts_injection": { "enabled": true, "instructions": "..." },
                    "notifications": { "idle": "Claude is idle", "awaiting_permission": "...", "error": "..." },
                    "first_launch_notice_dismissed": true,
                    "theme_override": null,
                    "background_override": null
                },
                {
                    "kind": "ai_tool",
                    "id": "aider",
                    "ai_tool_kind": "aider",
                    "builtin": true,
                    "name": "Aider",
                    "command": "aider",
                    "args": ["--model-metadata-file", ".aider.model.metadata.json"],
                    "cwd": null,
                    "env": { "USER_KEY": "user-value" },
                    "tts_injection": { "enabled": false, "instructions": "" },
                    "notifications": {
                        "idle": "Aider is idle",
                        "awaiting_permission": "Aider is awaiting permission",
                        "error": "Aider encountered an error"
                    },
                    "first_launch_notice_dismissed": false,
                    "theme_override": { "name": "Solarized Dark", "custom": null },
                    "background_override": null
                }
            ],
            "layout": {
                "tree": {
                    "type": "split",
                    "id": "split-1",
                    "direction": "horizontal",
                    "ratio": 0.5,
                    "first": { "type": "pane", "id": "pane-1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                    "second": { "type": "pane", "id": "pane-2", "tab_ids": ["aider"], "active_tab_id": "aider" }
                },
                "focused_pane_id": "pane-2"
            },
            "layout_presets": [
                {
                    "name": "Both AI",
                    "created_at": "2026-05-07T00:00:00Z",
                    "tree": { "type": "pane", "id": "pane-x", "tab_ids": ["claude", "aider"], "active_tab_id": "aider" }
                }
            ],
            "session": { "active_tab_id": "aider" },
            "terminal": {
                "theme": { "name": "Default", "custom": null },
                "background": {
                    "image": null,
                    "color": null,
                    "opacity": 0.4,
                    "blur": 0,
                    "size": "cover",
                    "position": "center",
                    "snapshot_lines": 2000,
                    "presets": [],
                    "preview_category_flips": true
                },
                "scrollback": { "ring_bytes": 262144, "persist": true, "restore_on_launch": true }
            }
        })
    }

    #[test]
    fn looks_v1_7_detects_v1_7_files() {
        let v = v1_7_with_aider_tab();
        assert!(looks_v1_7(&v));
    }

    #[test]
    fn v1_8_file_is_not_re_detected() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        assert!(!looks_v1_7(&v));
        // Idempotent on second pass.
        migrate_v1_7_to_v1_8(&mut v);
        assert!(!looks_v1_7(&v));
    }

    #[test]
    fn v1_7_to_v1_8_stamps_claude_local_defaults() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        let cl = v.get("claude_local").expect("claude_local present");
        assert_eq!(cl.get("base_url").unwrap(), "http://localhost:4000");
        assert_eq!(cl.get("auth_token").unwrap(), "sk-dummy");
        assert_eq!(cl.get("model_alias").unwrap(), "");
    }

    #[test]
    fn v1_7_to_v1_8_drops_ai_tool_kind_from_every_ai_tab() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        for tab in tabs {
            if tab.get("kind").and_then(Value::as_str) == Some("ai_tool") {
                assert!(
                    tab.get("ai_tool_kind").is_none(),
                    "ai_tool_kind should have been dropped"
                );
            }
        }
    }

    #[test]
    fn v1_7_to_v1_8_stamps_use_local_provider_on_every_ai_tab() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        let claude = &tabs[0];
        let claude_local = &tabs[1];
        assert_eq!(claude.get("use_local_provider").unwrap(), false);
        assert_eq!(claude_local.get("use_local_provider").unwrap(), true);
    }

    #[test]
    fn v1_7_to_v1_8_rewrites_aider_tab_in_place() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        let rewritten = &tabs[1];
        assert_eq!(rewritten.get("id").unwrap(), "claude-local");
        assert_eq!(rewritten.get("name").unwrap(), "Claude (local)");
        assert_eq!(rewritten.get("command").unwrap(), "claude");
        assert!(rewritten
            .get("args")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(rewritten.get("use_local_provider").unwrap(), true);
        // tts_injection re-enabled (V20: plain speak gate, no instructions).
        let tts = rewritten.get("tts_injection").unwrap();
        assert_eq!(tts.get("enabled").unwrap(), true);
        // Canonical aider notifications rewritten to claude-local.
        let n = rewritten.get("notifications").unwrap();
        assert_eq!(n.get("idle").unwrap(), "Claude (local) is idle");
        assert_eq!(
            n.get("awaiting_permission").unwrap(),
            "Claude (local) is awaiting permission"
        );
        assert_eq!(
            n.get("error").unwrap(),
            "Claude (local) encountered an error"
        );
    }

    #[test]
    fn v1_7_to_v1_8_preserves_user_env_on_rewritten_tab() {
        // The rewrite-in-place semantics promise to preserve per-tab env
        // (where the user typically set their local-LLM proxy URL).
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        let rewritten = &tabs[1];
        let env = rewritten.get("env").unwrap().as_object().unwrap();
        assert_eq!(env.get("USER_KEY").unwrap(), "user-value");
    }

    #[test]
    fn v1_7_to_v1_8_preserves_theme_override_on_rewritten_tab() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        let rewritten = &tabs[1];
        // theme_override was set on the aider tab in the fixture; should
        // carry through unchanged (rewrite touches identity fields, not
        // appearance).
        assert_eq!(
            rewritten
                .get("theme_override")
                .unwrap()
                .get("name")
                .unwrap(),
            "Solarized Dark"
        );
    }

    #[test]
    fn v1_7_to_v1_8_rewrites_layout_tree_aider_references() {
        let mut v = v1_7_with_aider_tab();
        migrate_v1_7_to_v1_8(&mut v);

        // layout.tree.second.tab_ids and active_tab_id rewritten.
        let layout = v.get("layout").unwrap();
        let second = layout.get("tree").unwrap().get("second").unwrap();
        let tab_ids = second.get("tab_ids").unwrap().as_array().unwrap();
        assert_eq!(tab_ids[0], "claude-local");
        assert_eq!(second.get("active_tab_id").unwrap(), "claude-local");

        // layout_presets[0].tree references rewritten.
        let preset_tree = v.get("layout_presets").unwrap().as_array().unwrap()[0]
            .get("tree")
            .unwrap();
        let preset_tab_ids = preset_tree.get("tab_ids").unwrap().as_array().unwrap();
        assert_eq!(preset_tab_ids[0], "claude");
        assert_eq!(preset_tab_ids[1], "claude-local");
        assert_eq!(preset_tree.get("active_tab_id").unwrap(), "claude-local");

        // session.active_tab_id rewritten.
        assert_eq!(
            v.get("session").unwrap().get("active_tab_id").unwrap(),
            "claude-local"
        );
    }

    #[test]
    fn v1_7_to_v1_8_handles_missing_aider_tab() {
        // A user who deleted their aider tab pre-migration: the rewrite
        // step finds nothing to rewrite, but the migration still stamps
        // claude_local and use_local_provider on the remaining tabs. The
        // integrity check (run after migration) restores claude-local.
        let mut v = v1_7_with_aider_tab();
        // Drop the aider tab.
        if let Some(arr) = v
            .as_object_mut()
            .and_then(|o| o.get_mut("tabs"))
            .and_then(Value::as_array_mut)
        {
            arr.retain(|t| t.get("id").and_then(Value::as_str) != Some("aider"));
        }
        migrate_v1_7_to_v1_8(&mut v);
        assert!(v.get("claude_local").is_some());
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        for tab in tabs {
            if tab.get("kind").and_then(Value::as_str) == Some("ai_tool") {
                assert!(tab.get("use_local_provider").unwrap().is_boolean());
                assert!(tab.get("ai_tool_kind").is_none());
            }
        }
    }

    #[test]
    fn v1_2_cascades_through_v1_3_v1_4_v1_5_v1_6_v1_7_and_v1_8() {
        // Full cascade from a v1.2 file to v1.8 in one pass. Verifies
        // every step fires once and the final shape is the v1.8 schema.
        let mut v: Value = serde_json::from_str(
            r#"{
                "display": { "theme": "dark" },
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "ai_tool_kind": "claude_code", "name": "Claude" },
                    { "kind": "ai_tool", "id": "aider", "ai_tool_kind": "aider", "name": "Aider" },
                    { "kind": "shell", "id": "shell-default-1", "name": "Shell 1" }
                ],
                "session": { "active_tab_id": "aider" }
            }"#,
        )
        .unwrap();

        assert!(looks_v1_2(&v));
        migrate_v1_2_to_v1_3(&mut v);
        assert!(looks_v1_3(&v));
        migrate_v1_3_to_v1_4(&mut v);
        assert!(looks_v1_4(&v));
        migrate_v1_4_to_v1_5(&mut v);
        assert!(looks_v1_5(&v));
        migrate_v1_5_to_v1_6(&mut v);
        assert!(looks_v1_6(&v));
        migrate_v1_6_to_v1_7(&mut v);
        assert!(looks_v1_7(&v));
        migrate_v1_7_to_v1_8(&mut v);

        // Post-cascade invariants.
        assert!(!looks_v1_7(&v));
        assert!(v.get("claude_local").is_some());
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        // aider was rewritten to claude-local.
        assert_eq!(tabs[1].get("id").unwrap(), "claude-local");
        assert_eq!(tabs[1].get("use_local_provider").unwrap(), true);
        assert!(tabs[1].get("ai_tool_kind").is_none());
        assert_eq!(tabs[0].get("use_local_provider").unwrap(), false);
        // The v1.2 → v1.3 migration moves session.active_tab_id into
        // the layout's per-pane active_tab_id, so by v1.8 the session
        // field is null. The layout's active tab should reflect the
        // rewritten id when the original active tab was aider.
        let layout = v.get("layout").expect("layout populated by v1.2 → v1.3");
        let tree = layout.get("tree").unwrap();
        // The v1.2 → v1.3 migration synthesizes a single root pane
        // with all tabs in order. After v1.7 → v1.8 the aider id has
        // become claude-local everywhere it appeared.
        let tab_ids = tree.get("tab_ids").unwrap().as_array().unwrap();
        assert!(tab_ids.iter().any(|t| t.as_str() == Some("claude-local")));
        assert!(!tab_ids.iter().any(|t| t.as_str() == Some("aider")));
    }

    fn v1_8_with_tabs(claude: bool, claude_local: bool) -> Value {
        let mut tabs: Vec<Value> = Vec::new();
        if claude {
            tabs.push(json!({
                "kind": "ai_tool",
                "id": "claude",
                "name": "Claude",
                "use_local_provider": false,
            }));
        }
        if claude_local {
            tabs.push(json!({
                "kind": "ai_tool",
                "id": "claude-local",
                "name": "Claude (local)",
                "use_local_provider": true,
            }));
        }
        tabs.push(json!({
            "kind": "shell",
            "id": "shell-default-1",
            "name": "Shell 1",
        }));
        json!({
            "tabs": tabs,
            "claude_local": {
                "base_url": "http://localhost:4000",
                "auth_token": "sk-dummy",
                "model_alias": "",
            },
        })
    }

    #[test]
    fn v1_8_to_v1_9_infers_both_when_both_tabs_present() {
        let mut v = v1_8_with_tabs(true, true);
        assert!(looks_v1_8(&v));
        migrate_v1_8_to_v1_9(&mut v);
        assert_eq!(v.get("claude_tabs_enabled").unwrap(), "both");
        assert!(!looks_v1_8(&v));
    }

    #[test]
    fn v1_8_to_v1_9_infers_cloud_when_only_claude() {
        let mut v = v1_8_with_tabs(true, false);
        migrate_v1_8_to_v1_9(&mut v);
        assert_eq!(v.get("claude_tabs_enabled").unwrap(), "cloud");
    }

    #[test]
    fn v1_8_to_v1_9_infers_local_when_only_claude_local() {
        let mut v = v1_8_with_tabs(false, true);
        migrate_v1_8_to_v1_9(&mut v);
        assert_eq!(v.get("claude_tabs_enabled").unwrap(), "local");
    }

    #[test]
    fn v1_8_to_v1_9_defaults_cloud_when_neither_tab_present() {
        let mut v = v1_8_with_tabs(false, false);
        migrate_v1_8_to_v1_9(&mut v);
        assert_eq!(v.get("claude_tabs_enabled").unwrap(), "cloud");
    }

    #[test]
    fn v1_9_file_is_not_re_detected() {
        let mut v = v1_8_with_tabs(true, true);
        migrate_v1_8_to_v1_9(&mut v);
        assert!(!looks_v1_8(&v));
        // Idempotent on second pass.
        migrate_v1_8_to_v1_9(&mut v);
        assert!(!looks_v1_8(&v));
    }

    // --- v1.9 → v1.10 -------------------------------------------------------

    #[test]
    fn v1_9_to_v1_10_stamps_schema_version() {
        let mut v = v1_8_with_tabs(true, true);
        migrate_v1_8_to_v1_9(&mut v);
        assert!(looks_v1_9(&v));
        migrate_v1_9_to_v1_10(&mut v);
        // The v1.9 → v1.10 step stamps the literal version 10 (the
        // version it produces), not the moving CURRENT_SCHEMA_VERSION.
        // The next step in the cascade (v1.10 → v1.11) bumps to current.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(10));
        assert!(!looks_v1_9(&v));
    }

    #[test]
    fn v1_10_file_is_not_re_detected() {
        // A fresh-schema file has the schema_version field already and
        // must not re-trigger the v1.9 detector.
        let v = json!({
            "schema_version": crate::settings::schema::CURRENT_SCHEMA_VERSION,
            "claude_tabs_enabled": "cloud",
            "tabs": [],
            "claude_local": { "base_url": "", "auth_token": "", "model_alias": "" },
        });
        assert!(!looks_v1_8(&v));
        assert!(!looks_v1_9(&v));
    }

    // --- v1.10 → v1.11 ------------------------------------------------------

    #[test]
    fn v1_10_to_v1_11_promotes_string_notifications_to_objects() {
        let mut v = json!({
            "schema_version": 10,
            "tabs": [
                {
                    "kind": "ai_tool",
                    "id": "claude",
                    "notifications": {
                        "idle": "Claude is idle",
                        "awaiting_permission": "",
                        "question": "Claude has a question",
                        "error": "Claude encountered an error"
                    }
                },
                {
                    "kind": "shell",
                    "id": "shell-1",
                    "notifications": {
                        "error": "boom",
                        "exited": ""
                    }
                }
            ]
        });
        assert!(looks_v1_10(&v));
        migrate_v1_10_to_v1_11(&mut v);
        let tabs = v.get("tabs").unwrap().as_array().unwrap();
        let idle = tabs[0].get("notifications").unwrap().get("idle").unwrap();
        assert_eq!(idle.get("enabled").unwrap(), true);
        assert_eq!(idle.get("text").unwrap(), "Claude is idle");
        let perm = tabs[0]
            .get("notifications")
            .unwrap()
            .get("awaiting_permission")
            .unwrap();
        assert_eq!(perm.get("enabled").unwrap(), false);
        assert_eq!(perm.get("text").unwrap(), "");
        let exited = tabs[1].get("notifications").unwrap().get("exited").unwrap();
        assert_eq!(exited.get("enabled").unwrap(), false);
        // The v1.10 → v1.11 step stamps the literal version 11 (the version
        // it produces). The next step in the cascade (v1.11 → v1.12) bumps
        // to current.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(11));
        assert!(!looks_v1_10(&v));
    }

    // --- v1.11 → v1.12 ------------------------------------------------------

    #[test]
    fn v1_11_to_v1_12_promotes_margin_scalar_to_xy_object() {
        let mut v = json!({
            "schema_version": 11,
            "avatar": {
                "margin_px": 24,
            }
        });
        assert!(looks_v1_11(&v));
        migrate_v1_11_to_v1_12(&mut v);
        let avatar = v.get("avatar").unwrap();
        assert!(
            avatar.get("margin_px").is_none(),
            "legacy scalar should be removed",
        );
        let margin = avatar.get("margin").unwrap();
        assert_eq!(margin.get("x_px").unwrap(), 24);
        assert_eq!(margin.get("y_px").unwrap(), 24);
        // V1.11 → V1.12 stamps the literal 12 (frozen — V1.13 added
        // a follow-on step that bumps to current).
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(12));
        assert!(!looks_v1_11(&v));
    }

    #[test]
    fn v1_11_to_v1_12_leaves_margin_absent_when_legacy_field_missing() {
        // No legacy `margin_px` → the migration must NOT synthesize a `margin`
        // object. Doing so pollutes the custom overlay (which holds only diffs
        // from global) with a phantom default that then overrides global forever.
        // The schema's `AvatarMargin::default()` supplies the value at parse time.
        let mut v = json!({
            "schema_version": 11,
            "avatar": {}
        });
        migrate_v1_11_to_v1_12(&mut v);
        assert!(v.get("avatar").unwrap().get("margin").is_none());
        // The version stamp still advances regardless.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(12));
    }

    #[test]
    fn v1_12_to_v1_13_renames_tui_theme_to_tui_orange() {
        let mut v = json!({
            "schema_version": 12,
            "ui": { "theme": "tui" },
        });
        assert!(looks_v1_12(&v));
        migrate_v1_12_to_v1_13(&mut v);
        assert_eq!(v.get("ui").unwrap().get("theme").unwrap(), "tui-orange");
        // v1.12 → v1.13 stamps 13, even when CURRENT_SCHEMA_VERSION has
        // moved further. The v1.13 → v1.14 step is what brings the file
        // up to the current version on the cascade.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(13));
        assert!(!looks_v1_12(&v));
    }

    #[test]
    fn v1_13_to_v1_14_maps_cloud_to_claude_only() {
        let mut v = json!({
            "schema_version": 13,
            "claude_tabs_enabled": "cloud",
        });
        assert!(looks_v1_13(&v));
        migrate_v1_13_to_v1_14(&mut v);
        assert!(v.get("claude_tabs_enabled").is_none());
        assert_eq!(v.get("enabled_ai_tabs").unwrap(), &json!(["claude"]),);
        // This step stamps a literal 14 (the final v14 → v15 step bumps to
        // CURRENT_SCHEMA_VERSION); see the comment in migrate_v1_13_to_v1_14.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(14),);
        // aider_local defaults stamped.
        let aider_local = v.get("aider_local").unwrap();
        assert_eq!(
            aider_local.get("base_url").unwrap(),
            "http://localhost:11434/v1"
        );
        assert_eq!(aider_local.get("auth_token").unwrap(), "ollama");
        assert_eq!(aider_local.get("model").unwrap(), "");
        assert!(!looks_v1_13(&v));
    }

    #[test]
    fn v1_13_to_v1_14_maps_local_to_claude_local_only() {
        let mut v = json!({
            "schema_version": 13,
            "claude_tabs_enabled": "local",
        });
        migrate_v1_13_to_v1_14(&mut v);
        assert_eq!(v.get("enabled_ai_tabs").unwrap(), &json!(["claude-local"]),);
    }

    #[test]
    fn v1_13_to_v1_14_maps_both_to_claude_pair() {
        let mut v = json!({
            "schema_version": 13,
            "claude_tabs_enabled": "both",
        });
        migrate_v1_13_to_v1_14(&mut v);
        assert_eq!(
            v.get("enabled_ai_tabs").unwrap(),
            &json!(["claude", "claude-local"]),
        );
    }

    #[test]
    fn v1_13_to_v1_14_preserves_user_aider_local_settings() {
        // A user who hand-edited their settings to point at a different
        // local proxy should not have their aider_local block clobbered
        // by the migration. `entry().or_insert` keeps the user value.
        let mut v = json!({
            "schema_version": 13,
            "claude_tabs_enabled": "cloud",
            "aider_local": {
                "base_url": "http://my-host:8080/v1",
                "auth_token": "secret",
                "model": "qwen3:14b",
            },
        });
        migrate_v1_13_to_v1_14(&mut v);
        let al = v.get("aider_local").unwrap();
        assert_eq!(al.get("base_url").unwrap(), "http://my-host:8080/v1");
        assert_eq!(al.get("model").unwrap(), "qwen3:14b");
    }

    #[test]
    fn v1_12_to_v1_13_leaves_other_themes_alone() {
        // A v1.12 user already on modern-dark — only the schema_version
        // should be bumped; the theme string stays as-is.
        let mut v = json!({
            "schema_version": 12,
            "ui": { "theme": "modern-dark" },
        });
        migrate_v1_12_to_v1_13(&mut v);
        assert_eq!(v.get("ui").unwrap().get("theme").unwrap(), "modern-dark");
    }

    #[test]
    fn v1_14_to_v1_15_injects_broot_tab() {
        let mut v = json!({
            "schema_version": 14,
            "tabs": [
                { "kind": "ai_tool", "id": "claude" },
                { "kind": "shell", "id": "shell-default-1" },
            ],
        });
        assert!(looks_v1_14(&v));
        migrate_v1_14_to_v1_15(&mut v);

        let tabs = v.get("tabs").and_then(Value::as_array).unwrap();
        let broot = tabs
            .iter()
            .find(|t| t.get("id").and_then(Value::as_str) == Some("shell-broot"))
            .expect("broot tab injected");
        assert_eq!(broot.get("kind").unwrap(), "shell");
        assert_eq!(broot.get("command").unwrap(), "broot");
        assert_eq!(broot.get("args").unwrap(), &json!(["-g"]));
        assert_eq!(broot.get("cwd").unwrap(), &Value::Null);
        assert_eq!(broot.get("builtin").unwrap(), &Value::Bool(true));
        // Existing tabs preserved, broot appended at the end.
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[2].get("id").unwrap(), "shell-broot");

        // Stamps a literal 15 (this is no longer the terminal step; v15 → v16
        // follows and removes the broot tab again).
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(15));
        assert!(!looks_v1_14(&v));
        assert!(looks_v1_15(&v));
    }

    #[test]
    fn v1_15_to_v1_16_removes_broot_tab() {
        // V16 retires the auto-seeded broot builtin: the cascade drops the
        // `shell-broot` entry, leaving the user's other tabs untouched.
        let mut v = json!({
            "schema_version": 15,
            "tabs": [
                { "kind": "ai_tool", "id": "claude" },
                { "kind": "shell", "id": "shell-default-1" },
                { "kind": "shell", "id": "shell-broot", "command": "broot", "args": ["-g"] },
            ],
        });
        assert!(looks_v1_15(&v));
        migrate_v1_15_to_v1_16(&mut v);

        let tabs = v.get("tabs").and_then(Value::as_array).unwrap();
        assert_eq!(tabs.len(), 2);
        assert!(tabs
            .iter()
            .all(|t| t.get("id").and_then(Value::as_str) != Some("shell-broot")));
        assert_eq!(tabs[0].get("id").unwrap(), "claude");
        assert_eq!(tabs[1].get("id").unwrap(), "shell-default-1");

        // This step stamps a *literal* 16 so the v16 → v17 step (V8-02
        // backends) can match next in the cascade.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(16));
        assert!(!looks_v1_15(&v));
    }

    #[test]
    fn v1_15_to_v1_16_is_idempotent_without_broot() {
        // A file that never had (or already lost) a broot tab is unchanged
        // beyond the version stamp.
        let mut v = json!({
            "schema_version": 15,
            "tabs": [ { "kind": "shell", "id": "shell-default-1" } ],
        });
        migrate_v1_15_to_v1_16(&mut v);
        let tabs = v.get("tabs").and_then(Value::as_array).unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].get("id").unwrap(), "shell-default-1");
        assert!(!looks_v1_15(&v));
    }

    #[test]
    fn v1_14_to_v1_15_is_idempotent_on_existing_broot() {
        // A file that already has a shell-broot tab (fresh-install seed, or
        // a re-run) must not get a duplicate.
        let mut v = json!({
            "schema_version": 14,
            "tabs": [
                { "kind": "shell", "id": "shell-broot", "command": "broot", "args": ["-g"] },
            ],
        });
        migrate_v1_14_to_v1_15(&mut v);
        let count = v
            .get("tabs")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter(|t| t.get("id").and_then(Value::as_str) == Some("shell-broot"))
            .count();
        assert_eq!(count, 1, "no duplicate broot tab");
        assert!(!looks_v1_14(&v));
    }

    #[test]
    fn v1_16_to_v1_17_folds_server_command_into_one_local_backend() {
        // A V8-01 single-local config migrates into one Local backend that
        // preserves the user's command + autostart.
        let mut v = json!({
            "schema_version": 16,
            "offload": {
                "enabled": true,
                "autostart": true,
                "server_command": "llama-server --model q4.gguf --jinja -np 2",
            },
        });
        migrate_v1_16_to_v1_17(&mut v);
        let backends = v
            .pointer("/offload/backends")
            .and_then(Value::as_array)
            .expect("backends array");
        assert_eq!(backends.len(), 1);
        let b = &backends[0];
        assert_eq!(b.get("name").unwrap(), "local");
        assert_eq!(b.get("tier").unwrap(), "quality");
        assert_eq!(b.pointer("/kind/type").unwrap(), "local");
        assert_eq!(
            b.pointer("/kind/server_command").unwrap(),
            "llama-server --model q4.gguf --jinja -np 2"
        );
        assert_eq!(b.pointer("/kind/autostart").unwrap(), &json!(true));
        assert_eq!(b.pointer("/tool_scope/mode").unwrap(), "all");
        // This step stamps a *literal* 17 (the v17→v18 step runs next in the
        // full cascade); see the comment in `migrate_v1_16_to_v1_17`.
        assert_eq!(v.get("schema_version").and_then(Value::as_u64), Some(17));
        assert!(!looks_v1_16(&v));
    }

    #[test]
    fn v1_16_to_v1_17_empty_command_yields_empty_pool() {
        // An unconfigured offload block (feature off) migrates to an empty
        // pool, not a bogus backend with a blank command.
        let mut v = json!({
            "schema_version": 16,
            "offload": { "enabled": false, "server_command": "" },
        });
        migrate_v1_16_to_v1_17(&mut v);
        let backends = v
            .pointer("/offload/backends")
            .and_then(Value::as_array)
            .expect("backends array");
        assert!(backends.is_empty());
    }

    #[test]
    fn v1_16_to_v1_17_is_idempotent_when_backends_present() {
        // A file that already has a populated pool is not clobbered.
        let mut v = json!({
            "schema_version": 16,
            "offload": {
                "server_command": "llama-server --jinja",
                "backends": [ { "name": "main", "kind": { "type": "local" } } ],
            },
        });
        migrate_v1_16_to_v1_17(&mut v);
        let backends = v
            .pointer("/offload/backends")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].get("name").unwrap(), "main");
        assert!(!looks_v1_16(&v));
    }

    #[test]
    fn detect_entry_version_returns_lowest_matching_detector() {
        // V0.6+ contract: `detect_entry_version` returns the lowest-version
        // detector that matches, so the cascade writes one backup labelled
        // by the user's actual on-disk version. The v1_8_with_tabs helper
        // is intentionally minimal — it lacks v1.3+ markers, so it matches
        // the v1.2 detector first. That's correct behavior: the cascade
        // will fill in v1.3+ markers as it walks forward.
        let minimal = v1_8_with_tabs(true, true);
        assert_eq!(detect_entry_version(&minimal), Some("v1.2"));

        // Empty value never matches any detector.
        assert_eq!(detect_entry_version(&json!({})), None);
        // A current-shape file (has `schema_version`) skips the cascade.
        let current = json!({
            "schema_version": crate::settings::schema::CURRENT_SCHEMA_VERSION,
            "tabs": [],
            "layout": null,
            "terminal": {"background": {"presets": []}, "scrollback": {}},
            "claude_local": {},
            "aider_local": {},
            "enabled_ai_tabs": ["claude"],
        });
        assert_eq!(detect_entry_version(&current), None);
    }
}

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
    write_backup(path, entry.unwrap(), value)?;

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
    MigrationStep { from_version: "v1.0", detect: looks_v1, transform: migrate_v1_to_v1_2 },
    MigrationStep { from_version: "v1.1", detect: looks_v1_1, transform: migrate_v1_1_to_v1_2 },
    MigrationStep { from_version: "v1.2", detect: looks_v1_2, transform: migrate_v1_2_to_v1_3_step },
    MigrationStep { from_version: "v1.3", detect: looks_v1_3, transform: migrate_v1_3_to_v1_4_step },
    MigrationStep { from_version: "v1.4", detect: looks_v1_4, transform: migrate_v1_4_to_v1_5_step },
    MigrationStep { from_version: "v1.5", detect: looks_v1_5, transform: migrate_v1_5_to_v1_6_step },
    MigrationStep { from_version: "v1.6", detect: looks_v1_6, transform: migrate_v1_6_to_v1_7_step },
    MigrationStep { from_version: "v1.7", detect: looks_v1_7, transform: migrate_v1_7_to_v1_8_step },
    MigrationStep { from_version: "v1.8", detect: looks_v1_8, transform: migrate_v1_8_to_v1_9_step },
    MigrationStep { from_version: "v1.9", detect: looks_v1_9, transform: migrate_v1_9_to_v1_10_step },
    MigrationStep { from_version: "v1.10", detect: looks_v1_10, transform: migrate_v1_10_to_v1_11_step },
    MigrationStep { from_version: "v1.11", detect: looks_v1_11, transform: migrate_v1_11_to_v1_12_step },
    MigrationStep { from_version: "v1.12", detect: looks_v1_12, transform: migrate_v1_12_to_v1_13_step },
    MigrationStep { from_version: "v1.13", detect: looks_v1_13, transform: migrate_v1_13_to_v1_14_step },
    MigrationStep { from_version: "v1.14", detect: looks_v1_14, transform: migrate_v1_14_to_v1_15_step },
    MigrationStep { from_version: "v1.15", detect: looks_v1_15, transform: migrate_v1_15_to_v1_16_step },
    MigrationStep { from_version: "v1.16", detect: looks_v1_16, transform: migrate_v1_16_to_v1_17_step },
    MigrationStep { from_version: "v1.17", detect: looks_v1_17, transform: migrate_v1_17_to_v1_18_step },
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
        .unwrap_or_else(|| serde_json::to_value(default_claude_tab()).expect("encode claude default"));
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
        "tts_injection": { "enabled": true, "instructions": crate::tts::RUNTIME_SYSTEM_PROMPT },
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

    if let Some(bg) = terminal.get_mut("background").and_then(Value::as_object_mut) {
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
                obj.insert(
                    "id".to_string(),
                    Value::String("claude-local".to_string()),
                );
                obj.insert("name".to_string(), Value::String("Claude (local)".to_string()));
                obj.insert("command".to_string(), Value::String("claude".to_string()));
                obj.insert("args".to_string(), Value::Array(Vec::new()));
                obj.insert("use_local_provider".to_string(), Value::Bool(true));
                // Aider's default had tts_injection.enabled=false; the
                // rewritten tab is Claude, which does honor TTS injection.
                if let Some(tts) = obj
                    .get_mut("tts_injection")
                    .and_then(Value::as_object_mut)
                {
                    tts.insert("enabled".to_string(), Value::Bool(true));
                    let needs_default = tts
                        .get("instructions")
                        .and_then(Value::as_str)
                        .map(str::is_empty)
                        .unwrap_or(true);
                    if needs_default {
                        tts.insert(
                            "instructions".to_string(),
                            Value::String(crate::tts::RUNTIME_SYSTEM_PROMPT.to_string()),
                        );
                    }
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
                        ("error", "Aider encountered an error", "Claude (local) encountered an error"),
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
            if let Some(notifications) = tab
                .get_mut("notifications")
                .and_then(Value::as_object_mut)
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
        let legacy = avatar
            .remove("margin_px")
            .and_then(|v| v.as_u64())
            .unwrap_or(16);
        avatar.insert(
            "margin".to_string(),
            json!({
                "x_px": legacy,
                "y_px": legacy,
            }),
        );
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
            t.get("id").and_then(Value::as_str)
                == Some(crate::settings::schema::SHELL_BROOT_TAB_ID)
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
            t.get("id").and_then(Value::as_str)
                != Some(crate::settings::schema::SHELL_BROOT_TAB_ID)
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

    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(
            crate::settings::schema::CURRENT_SCHEMA_VERSION,
        )),
    );
}

// --- Backup helpers ---------------------------------------------------------

/// Write `<full-filename>.<from_version>.bak` next to the settings file. If
/// that name already exists (the user somehow rolled back and re-migrated),
/// append a unix timestamp to the suffix so the original backup survives.
/// Failure here aborts the migration — we never proceed without a
/// recoverable copy.
///
/// Backup filenames are built by *appending* to the full filename rather
/// than `with_extension`, which only knows the last dot — for the per-folder
/// overlay file `.ccimp.custom.config.json`, `with_extension` would consume
/// `config` as the extension and produce `.ccimp.custom.json.<ver>.bak`,
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
        AppError::Settings(format!(
            "backup write {} failed: {e}",
            target.display()
        ))
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
/// (including any embedded dots), so a backup of `.ccimp.custom.config.json`
/// becomes `.ccimp.custom.config.json.<suffix>` rather than
/// `.ccimp.custom.json.<suffix>`.
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
        assert_eq!(
            v["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION)
        );
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
        let v: Value = serde_json::from_str(
            r#"{
                "schema_version": 18,
                "tabs": [
                    { "kind": "ai_tool", "id": "claude", "name": "Claude" }
                ]
            }"#,
        )
        .unwrap();
        assert!(!looks_v1_2(&v), "schema_version-bearing file matched looks_v1_2");
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
        let v: Value = serde_json::from_str(
            r#"{ "tabs": [ { "kind": "ai_tool", "id": "claude" } ] }"#,
        )
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
        assert!(v
            .get("display")
            .and_then(|d| d.get("theme"))
            .is_none());

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
        let bg = v
            .get("terminal")
            .and_then(|t| t.get("background"))
            .unwrap();
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
            entry
                .get("notifications")
                .unwrap()
                .get("error")
                .unwrap(),
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
        assert!(rewritten.get("args").unwrap().as_array().unwrap().is_empty());
        assert_eq!(rewritten.get("use_local_provider").unwrap(), true);
        // tts_injection re-enabled with default instructions.
        let tts = rewritten.get("tts_injection").unwrap();
        assert_eq!(tts.get("enabled").unwrap(), true);
        assert!(
            tts.get("instructions").unwrap().as_str().unwrap().len() > 10,
            "tts_injection.instructions should have been seeded with the runtime prompt"
        );
        // Canonical aider notifications rewritten to claude-local.
        let n = rewritten.get("notifications").unwrap();
        assert_eq!(n.get("idle").unwrap(), "Claude (local) is idle");
        assert_eq!(
            n.get("awaiting_permission").unwrap(),
            "Claude (local) is awaiting permission"
        );
        assert_eq!(n.get("error").unwrap(), "Claude (local) encountered an error");
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
        let preset_tree = v
            .get("layout_presets")
            .unwrap()
            .as_array()
            .unwrap()[0]
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
        assert_eq!(
            v.get("schema_version").and_then(Value::as_u64),
            Some(12)
        );
        assert!(!looks_v1_11(&v));
    }

    #[test]
    fn v1_11_to_v1_12_falls_back_to_default_when_legacy_field_missing() {
        // A v1.11 file in which the user already had a partially-migrated
        // avatar block (e.g. settings hand-edits) — the migration should
        // still produce a valid `margin` object so the schema's
        // serde-default doesn't have to silently absorb the gap.
        let mut v = json!({
            "schema_version": 11,
            "avatar": {}
        });
        migrate_v1_11_to_v1_12(&mut v);
        let margin = v.get("avatar").unwrap().get("margin").unwrap();
        assert_eq!(margin.get("x_px").unwrap(), 16);
        assert_eq!(margin.get("y_px").unwrap(), 16);
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
        assert_eq!(
            v.get("enabled_ai_tabs").unwrap(),
            &json!(["claude"]),
        );
        // This step stamps a literal 14 (the final v14 → v15 step bumps to
        // CURRENT_SCHEMA_VERSION); see the comment in migrate_v1_13_to_v1_14.
        assert_eq!(
            v.get("schema_version").and_then(Value::as_u64),
            Some(14),
        );
        // aider_local defaults stamped.
        let aider_local = v.get("aider_local").unwrap();
        assert_eq!(aider_local.get("base_url").unwrap(), "http://localhost:11434/v1");
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
        assert_eq!(
            v.get("enabled_ai_tabs").unwrap(),
            &json!(["claude-local"]),
        );
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

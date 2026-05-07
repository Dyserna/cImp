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
use crate::settings::schema::{
    default_aider_tab, default_claude_tab, AIDER_TAB_ID, CLAUDE_TAB_ID, SHELL_DEFAULT_TAB_ID,
};
use crate::shell::ShellSpec;

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
    let mut changed = false;

    if looks_v1(value) {
        write_backup(path, "v1.0", value)?;
        migrate_v1_to_v1_2(value, default_shell);
        changed = true;
        // After v1 → v1.2 the file's `tabs` field is already the array
        // shape; the v1.1 branch below short-circuits naturally.
    } else if looks_v1_1(value) {
        write_backup(path, "v1.1", value)?;
        migrate_v1_1_to_v1_2(value, default_shell);
        changed = true;
    }

    // v1.2 → v1.3: detected as "tabs is array" (post-v1.2-shape) plus
    // "no `layout` field present". Either of the v1.0 / v1.1 branches
    // above coerces tabs into an array, so this branch is the second
    // pass for files that came in as v1.0 or v1.1 too — they get one
    // file rewrite, not two backup files.
    if looks_v1_2(value) {
        write_backup(path, "v1.2", value)?;
        migrate_v1_2_to_v1_3(value);
        changed = true;
    }

    // v1.3 → v1.4: V1.4-01 adds the top-level `terminal` group (with a
    // `theme` sub-group) and a per-tab `theme_override` field. Detected
    // as "post-v1.3 (layout key present) but no `terminal` key". The
    // dead `display.theme` field is dropped at the same time. Cascades
    // naturally for files coming through earlier branches.
    if looks_v1_3(value) {
        write_backup(path, "v1.3", value)?;
        migrate_v1_3_to_v1_4(value);
        changed = true;
    }

    // v1.4 → v1.5: V1.4-02 adds the `terminal.background` sub-group and
    // a per-tab `background_override` field. Detected as "has terminal
    // (v1.4) but lacks terminal.background (v1.5)". Cascades naturally
    // for files coming through earlier branches — by the time control
    // reaches here, the v1.3→v1.4 branch has already inserted
    // `terminal.theme`, so this branch fires once on every cold-start
    // file from v1.0 through v1.4.
    if looks_v1_4(value) {
        write_backup(path, "v1.4", value)?;
        migrate_v1_4_to_v1_5(value);
        changed = true;
    }

    // v1.5 → v1.6: V1.4-04 B adds `terminal.background.presets`. A
    // single new array field; no per-tab stamping needed because
    // presets live globally. Cascades naturally — the v1.4→v1.5
    // branch above stamps `terminal.background` first, then this
    // branch fires on the same value before flush.
    if looks_v1_5(value) {
        write_backup(path, "v1.5", value)?;
        migrate_v1_5_to_v1_6(value);
        changed = true;
    }

    // v1.6 → v1.7: V1.4-04 D adds `terminal.scrollback` (cross-restart
    // ring buffer settings) and explicitly stamps the V1.4-04 C.4
    // `terminal.background.preview_category_flips` flag. Detected as
    // "has terminal.background.presets (v1.6) but lacks
    // terminal.scrollback (v1.7)".
    if looks_v1_6(value) {
        write_backup(path, "v1.6", value)?;
        migrate_v1_6_to_v1_7(value);
        changed = true;
    }

    Ok(changed)
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
    let tabs_is_array = value
        .get("tabs")
        .map(|v| v.is_array())
        .unwrap_or(false);
    let layout_field_absent = !value
        .as_object()
        .map(|o| o.contains_key("layout"))
        .unwrap_or(false);
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
        .unwrap_or_else(|| serde_json::to_value(default_aider_tab()).expect("encode aider default"));
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
            AiToolBuiltin::Aider => AIDER_TAB_ID,
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
    entry.insert("builtin".to_string(), Value::Bool(true));
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
    let aider_entry = serde_json::to_value(default_aider_tab()).expect("encode aider default");
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

    // Insert default `terminal.theme` group. Matches the Default impl
    // on `TerminalThemeSettings` in schema.rs — keep these in sync.
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

// --- Backup helpers ---------------------------------------------------------

/// Write `<path>.<from_version>.bak` next to the settings file. If that name
/// already exists (the user somehow rolled back and re-migrated), append a
/// unix timestamp to the suffix so the original backup survives. Failure
/// here aborts the migration — we never proceed without a recoverable copy.
fn write_backup(path: &Path, from_version: &str, value: &Value) -> AppResult<()> {
    let primary = path.with_extension(format!("json.{from_version}.bak"));
    let target = if primary.exists() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        path.with_extension(format!("json.{from_version}.bak.{ts}"))
    } else {
        primary
    };

    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| AppError::Settings(format!("backup serialize: {e}")))?;
    fs::write(&target, bytes).map_err(|e| {
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

/// Move a corrupt settings file aside before resetting to defaults. Best-
/// effort: a failed move just logs and returns — we still want to reset.
pub fn quarantine_corrupt_file(path: &Path) {
    if !path.exists() {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target: PathBuf = path.with_extension(format!("json.corrupted.{ts}.bak"));
    if let Err(e) = fs::rename(path, &target) {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            target = %target.display(),
            "settings: could not quarantine corrupt file"
        );
    } else {
        tracing::warn!(
            quarantine = %target.display(),
            "settings: corrupt file moved aside; defaults will be written"
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
        assert_eq!(aider.get("id").unwrap(), AIDER_TAB_ID);

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
        assert_eq!(aider.get("id").unwrap(), AIDER_TAB_ID);
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
}

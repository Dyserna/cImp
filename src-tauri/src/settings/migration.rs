//! Settings file migrations: an on-disk file, forward one schema version at a
//! time, until it is the shape the typed `Settings` expects.
//!
//! Operates on untyped `serde_json::Value` so each version's transformation can
//! run as a pre-pass before serde's typed deserialize. [`MIGRATION_STEPS`] is
//! the whole design: one row per schema bump, each a `detect` that recognises
//! exactly its own version and a `transform` that produces the next. A file
//! entering at the oldest supported version cascades through every row in a
//! single pass, because each transform stamps the version the next row detects.
//!
//! **This ladder is append-only frozen history.** Every row describes a shape
//! that exists in some user's file, transcribed at the time it was current; a
//! row is never edited to be more general, more correct, or more like its
//! neighbours, because the file it reads did not change. New schema versions are
//! added at the end.
//!
//! **There is exactly one way to remove a row: raise a floor.**
//! [`MIN_GLOBAL_SCHEMA_VERSION`] (global files) and
//! [`MIN_OVERLAY_SCHEMA_VERSION`] (project overlays) name the oldest version
//! each path may be entered at. Rows below the floor are deleted; a file below
//! it is not migrated, not parsed and not overwritten — it is moved aside intact
//! and defaults are reseeded (`persistence::load_global`). Deleting rows without
//! moving the floor first is silent data loss: the file matches no detector,
//! parses anyway through `Settings`' container-level `#[serde(default)]`, and
//! comes back with everything those rows would have moved quietly defaulted.
//!
//! V42 R9 (issue #120) raised both floors to 30 and deleted v1.0 → v29 — the
//! whole pre-`schema_version` era, its presence-archaeology detectors, and the
//! Aider tab it kept alive.
//!
//! Backups are written with collision-rotation so a user who somehow rolls
//! back and re-migrates doesn't lose the original.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::shell::ShellSpec;

/// The oldest schema version the **overlay** cascade can be entered at.
///
/// Until V42 R9 this was 10, and it meant something narrower than the global
/// floor: below v1.10 the detectors were *presence archaeology* — "has
/// `claude_code`, has no `tabs`", "`tabs` is an object" — which key off keys a
/// **partial** overlay legitimately lacks, and which the v1.2 → v1.4 transforms
/// answered by inserting whole-object defaults. Entering the cascade there
/// rewrote a sparse diff into a full file.
///
/// R9 deleted those steps, so the two floors now coincide at
/// [`MIN_GLOBAL_SCHEMA_VERSION`] for the same blunt reason: there are no rows
/// below it. They stay separate constants because they answer different
/// questions and could move apart again — the overlay's is about what a *sparse*
/// value may be dragged through, the global's about what exists to drag it.
///
/// An overlay below this is left unmigrated rather than quarantined: it is a
/// diff, reconstructible from the global baseline plus the user's next save, and
/// nothing of theirs is lost by ignoring it (see [`migrate_overlay`]).
pub const MIN_OVERLAY_SCHEMA_VERSION: u64 = 30;

/// The oldest schema version the **global** settings file may be loaded at.
///
/// [`MIN_OVERLAY_SCHEMA_VERSION`] began life as a narrower rule about what a
/// *sparse* value may be dragged through. This floor is the blunt one: below it
/// there is no ladder. The global path (`persistence::load_global` →
/// [`migrate_if_needed`]) had **no floor at all**, which was survivable only
/// while every step back to v1.0 was still present.
///
/// Left to the ordinary path, a below-floor file does not fail loudly — it
/// succeeds quietly, which is worse. No detector matches it, so the cascade is a
/// no-op; `Settings` carries a container-level `#[serde(default)]`, so it then
/// deserializes cleanly with every field the deleted steps would have MOVED
/// silently reset to a default (the v29 → v30 `run_check` tool scopes, the
/// v33 → v34 audit roster, the v35 → v36 harness map); and the file is written
/// back still stamped at its own old version, so no launch ever warns again.
///
/// So a file below this floor is not loaded at all: it is moved aside INTACT and
/// defaults are reseeded in its place (`persistence::load_global`). That is the
/// one outcome that neither discards the user's file nor lies about what was
/// read. Raising this constant is the ONLY legal way to retire migration steps.
pub const MIN_GLOBAL_SCHEMA_VERSION: u64 = 30;

// The overlay floor may sit ABOVE the global one (a sparse value could need a
// stricter rule than a whole file) but never below it: below the global floor
// the rows do not exist, so an overlay entered there would be stamped, walked
// past every detector, and returned unchanged while claiming it had been
// considered. Compile-time because it is a property of the two constants.
const _: () = assert!(MIN_OVERLAY_SCHEMA_VERSION >= MIN_GLOBAL_SCHEMA_VERSION);

/// The `schema_version` a value states, if it states one.
pub fn stated_schema_version(value: &Value) -> Option<u64> {
    value.get("schema_version").and_then(Value::as_u64)
}

/// Whether `value` — a global settings file that has already parsed as JSON —
/// sits below [`MIN_GLOBAL_SCHEMA_VERSION`].
///
/// **A stated version of `None` counts as below.** `schema_version` arrived with
/// the v1.9 → v1.10 step, so a file that states none is a pre-v1.10 file — which
/// is precisely what the (now deleted) `looks_v1…` presence detectors existed to
/// recognise. It is *not* the fresh-install case: a fresh install has no file at
/// all, is seeded with defaults before this is ever consulted, and so never
/// reaches here.
///
/// It is also not the *corrupt* case. Corrupt means the bytes did not parse;
/// this value did. The two share a mechanism (move aside, reseed) and must not
/// share their wording — see `persistence::reseed_below_floor`.
pub fn below_global_floor(value: &Value) -> bool {
    stated_schema_version(value).is_none_or(|v| v < MIN_GLOBAL_SCHEMA_VERSION)
}

/// **Run the cascade on a project OVERLAY** (V40 Phase I, issue #107 item 5).
///
/// The overlay is a sparse diff against the global baseline, and until Phase I
/// it was never migrated at all — `persistence::load`'s step 2 said so, and
/// gave two correct reasons: the presence-archaeology detectors fire on the
/// keys a partial file legitimately lacks, and a value with no `schema_version`
/// re-migrates on every launch, growing `.bak` files without bound.
///
/// Both reasons are about *entering the cascade blind*. Neither survives being
/// told the version: this takes `from` as a parameter, refuses anything below
/// [`MIN_OVERLAY_SCHEMA_VERSION`], stamps the value so the detectors can match,
/// and strips the stamp again on the way out so the overlay never carries a
/// schema version into the merge. (The archaeology detectors are gone since V42
/// R9 — the floor is now simply "below this there are no steps" — but the stamp
/// still has to go in, because every remaining detector reads it.)
///
/// It writes **no backup and no file**: an overlay is reconstructible from the
/// global baseline plus the user's next save, and a `.bak` per launch beside a
/// user's project is the unbounded growth the old comment named. The migrated
/// shape reaches disk when the user next saves, through the ordinary `diff`.
///
/// The gap this closes is real and not hypothetical: a project that set
/// `claude_local.base_url` before schema 36 kept a top-level `claude_local`
/// block in its overlay, the global file moved that field to
/// `harness.claude.ext["local.base_url"]`, and the project's value then reached
/// nothing — a per-project setting that silently stopped applying, with the file
/// still on disk saying otherwise.
///
/// Returns whether anything changed.
pub fn migrate_overlay(overlay: &mut Value, from: u64, default_shell: &ShellSpec) -> bool {
    // Dropped on EVERY path below: an overlay's own stamp is an entry marker
    // for this function and must never survive into the merge, where it would
    // `deep_merge` over the global's `schema_version` and pin the merged
    // `Settings` below whatever the global file actually reached.
    let stamped = overlay
        .as_object_mut()
        .and_then(|r| r.remove("schema_version"))
        .is_some();
    let current = crate::settings::schema::CURRENT_SCHEMA_VERSION as u64;
    if from >= current {
        return stamped;
    }
    if from < MIN_OVERLAY_SCHEMA_VERSION {
        tracing::warn!(
            from,
            min = MIN_OVERLAY_SCHEMA_VERSION,
            "settings: project overlay is older than the migration floor; leaving it unmigrated. \
             It is a diff, so nothing of the user's is lost — the global baseline answers, and \
             their next save rewrites it in the current shape"
        );
        return stamped;
    }
    let Some(root) = overlay.as_object_mut() else {
        return stamped;
    };
    let before = Value::Object(root.clone());
    // Stamped so the detectors — every one of which reads `schema_version` —
    // can match.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(from)),
    );
    for step in MIGRATION_STEPS {
        if (step.detect)(overlay) {
            (step.transform)(overlay, default_shell);
        }
    }
    // No force-stamp twin of `migrate_if_needed`'s: an overlay that did not
    // reach `current` is one whose steps found nothing of theirs in it, which is
    // the ordinary case for a sparse diff. Stamping it would only put a key into
    // the merge that must not be there.
    if let Some(root) = overlay.as_object_mut() {
        root.remove("schema_version");
    }
    stamped || *overlay != before
}

/// Detect file shape and run the appropriate transform on `value`. Returns
/// `Ok(true)` if the file changed shape (caller should write back to disk),
/// `Ok(false)` if the file was already current — or is **below
/// [`MIN_GLOBAL_SCHEMA_VERSION`]**, in which case nothing is migrated, backed
/// up, or stamped and the caller is expected to have quarantined it already.
/// `Err` if a backup write failed. Backup-write failure aborts migration
/// loudly — we never proceed without a recoverable copy.
pub fn migrate_if_needed(
    value: &mut Value,
    path: &Path,
    default_shell: &ShellSpec,
) -> AppResult<bool> {
    // **The global floor, enforced here as well as at the call site** (V42 R9,
    // issue #120). `persistence::load_global` quarantines a below-floor file
    // before it ever reaches this function; this is the second lock, and it is
    // the one that protects the fixpoint guard at the bottom. Without it an old
    // file would fall through every remaining detector, reach the force-stamp,
    // and be rewritten as CURRENT with everything the deleted steps would have
    // moved silently defaulted — a file that now *claims* to be current and so
    // can never be recognised as old again. Refusing without stamping leaves the
    // file's own version on disk, so the next launch reaches the same verdict
    // instead of a healed-looking lie.
    if below_global_floor(value) {
        tracing::error!(
            stated = ?stated_schema_version(value),
            floor = MIN_GLOBAL_SCHEMA_VERSION,
            path = %path.display(),
            "settings migration: file is below the migration floor; refusing to migrate it, \
             back it up, or stamp it — the caller quarantines and reseeds"
        );
        return Ok(false);
    }

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
        from_version: "v30",
        detect: looks_v30,
        transform: migrate_v30_to_v31_step,
    },
    MigrationStep {
        from_version: "v31",
        detect: looks_v31,
        transform: migrate_v31_to_v32_step,
    },
    MigrationStep {
        from_version: "v32",
        detect: looks_v32,
        transform: migrate_v32_to_v33_step,
    },
    MigrationStep {
        from_version: "v33",
        detect: looks_v33,
        transform: migrate_v33_to_v34_step,
    },
    MigrationStep {
        from_version: "v34",
        detect: looks_v34,
        transform: migrate_v34_to_v35_step,
    },
    MigrationStep {
        from_version: "v35",
        detect: looks_v35,
        transform: migrate_v35_to_v36_step,
    },
    MigrationStep {
        from_version: "v36",
        detect: looks_v36,
        transform: migrate_v36_to_v37_step,
    },
    MigrationStep {
        from_version: "v37",
        detect: looks_v37,
        transform: migrate_v37_to_v38_step,
    },
];

fn looks_v30(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 30)
}

fn migrate_v30_to_v31_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v30_to_v31(value)
}

/// V30 → V31: pure version stamp for the V33 Phase E LAN-auth fields.
///
/// V33 Phase E added three additive string fields, all defaulting to `""`:
/// `graph.embedding_auth_token`, `offload.mcp_servers[].auth_token`, and
/// `auth_token` on `OffloadBackendKind::Local`. Empty means "send no
/// `Authorization` header", which is byte-for-byte the pre-V33 request, so an
/// existing v30 file round-trips with every LAN client behaving exactly as
/// before and **no data transform is needed** (see the schema tests
/// `local_backend_kind_defaults_auth_token`,
/// `mcp_server_config_defaults_and_redacts_auth_token`).
///
/// It is stamped anyway, following the v23 → v24 and v28 → v29 precedent for
/// additive-only changes, and for one forward-looking reason of its own: this
/// is the release after which a settings file may contain **cleartext bearer
/// tokens for LAN services**. `docs/FUTURE-FEATURES-keyring.md` plans moving
/// house secrets to the OS keychain; that migration needs a version boundary to
/// gate on ("could this file carry one?"), and an un-versioned addition leaves
/// it with nothing to test but the presence of the keys themselves.
///
/// Idempotent: a second pass finds `schema_version == 31` so `looks_v30` is
/// false.
fn migrate_v30_to_v31(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    // Stamps a *literal* 31 (not `CURRENT_SCHEMA_VERSION`): the v31 → v32 step
    // runs next in the same cascade pass and gates on `schema_version == 31`.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(31u8)),
    );
}

fn looks_v31(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 31)
}

fn migrate_v31_to_v32_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v31_to_v32(value)
}

/// V31 → V32: pure version stamp for the V37 MCP registry fields.
///
/// V37 Phase A (contract C2) adds four additive fields, every one of which
/// deserializes from an absent key to the value that reproduces pre-V37
/// behaviour exactly:
///
/// * `offload.mcp_servers[].enabled` → `true` (the server exists),
/// * `offload.mcp_servers[].origin` → `external` (unknown provenance is
///   untrusted — the safe direction, and metadata-only in V37),
/// * `offload.mcp_categories` → `[]` (no categories),
/// * `offload.mcp_activation` → `{categories:{}, servers:{}}` (no overrides).
///
/// **The C2 invariant this step exists to protect**: the *effective tool
/// surface* of every existing config is unchanged after migration. It holds
/// because the effective-enable predicate
/// (`offload::mcp_host::effective_enable`) reduces to the server's own
/// `enabled` when no category contains it — and after this step no category
/// contains anything, because there are no categories. So there is **no data
/// transform to do**; the schema tests
/// `mcp_server_config_defaults_origin_and_enabled` and
/// `offload_settings_default_registry_is_empty` pin the defaults this relies on.
///
/// It is stamped anyway, following the v23 → v24, v28 → v29 and v30 → v31
/// precedent for additive-only changes: the marker is what a later step gates
/// on, and what tells a future reader whether a file could carry a category
/// list at all.
///
/// Idempotent: a second pass finds `schema_version == 32` so `looks_v31` is
/// false.
fn migrate_v31_to_v32(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    // Stamps a *literal* 32 (not `CURRENT_SCHEMA_VERSION`): the v32 → v33 step
    // runs next in the same cascade pass and gates on `schema_version == 32`.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(32u8)),
    );
}

fn looks_v32(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 32)
}

fn migrate_v32_to_v33_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v32_to_v33(value)
}

/// V31 → V33: pure version stamp for the V38 `tool_plugins` container.
///
/// **Nothing moves.** V38 Phase B adds one additive
/// [`ToolPluginsSettings`](crate::settings::schema::ToolPluginsSettings) block
/// carrying `#[serde(default)]`, so an existing v31 file deserializes with an
/// empty container and behaves byte-for-byte as before — there is no data to
/// transform, and every consumer of an empty container sees "no plugins
/// configured", which is the truth on a machine that has never had one.
///
/// It is stamped anyway, following the v23 → v24 / v28 → v29 / v30 → v31
/// precedent for additive-only changes, and for one reason of its own: this is
/// the version boundary the LATER move gates on. Phase E migrates
/// `code_audit.tools` into this container in the same commit that switches the
/// reader, as a v33 → v34 step — and that step needs a version to detect. An
/// unversioned addition would leave it testing for the presence of keys the
/// user may legitimately not have.
///
/// Deliberately NOT touching `code_audit`: every intermediate tree has to stay
/// releasable (the user cuts RCs from it), and a container that exists but is
/// not yet read by anything is releasable, while a half-moved `code_audit` is
/// not.
fn migrate_v32_to_v33(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    // Stamps a *literal* 33 (not `CURRENT_SCHEMA_VERSION`): the v33 → v34 step
    // runs next in the same cascade pass and gates on `schema_version == 33`.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(33u8)),
    );
}

fn looks_v33(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 33)
}

fn migrate_v33_to_v34_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v33_to_v34(value)
}

/// The fourteen built-in audit tool ids, and nothing else this step needs to
/// know about them.
///
/// **Deliberately a self-contained list of strings rather than a read of
/// `AuditToolId` or of the embedded manifest** (ruling R4). A migration step is
/// frozen history: it describes a file shape that existed on 2026-08-19, and it
/// has to keep describing it after the roster gains a fifteenth tool, loses one,
/// or renames the plugin it lives in. A step that read today's authority would
/// silently change what it migrates every time that authority moved — which is
/// how a migration starts producing a different result for the same input file
/// on two different releases.
///
/// The ids are the pre-v34 `code_audit.tools[].id` wire names, which are also
/// the tool ids in `plugins/builtin/cimp-audit.json` — that identity is what
/// makes this a move of the STORAGE rather than a rename of the tools, and
/// `the_v34_migration_ids_are_the_shipped_roster` checks it has not quietly
/// stopped being true.
const V34_AUDIT_TOOL_IDS: &[&str] = &[
    "osv-scanner",
    "gitleaks",
    "semgrep",
    "oxlint",
    "golangci-lint",
    "ruff",
    "cppcheck",
    "typos",
    "eslint",
    "pmd",
    "dotnet-analyzers",
    "knip",
    "cargo-machete",
    "semgrep-quality",
];

/// The `tool_plugins` key the v34 container writes the built-in audit tools
/// under. A literal for the same reason the id list is one.
const V34_AUDIT_PLUGIN_KEY: &str = "cimp-audit@1";

/// V33 → V34: move `code_audit.tools` into the `tool_plugins` container.
///
/// V38 Phase E turned the fourteen built-in scanners into embedded plugin
/// manifests, so the array that configured them has no field to deserialize
/// into any more. This step is what stops that being a silent reset of
/// everybody's audit configuration.
///
/// The mapping, field by field:
///
/// | v33 `code_audit.tools[]` | v34 |
/// |---|---|
/// | `enabled` | `tool_plugins.plugins["cimp-audit@1"].tools[<id>].enabled` |
/// | `timeout_secs` | …`.timeout_secs` |
/// | `extra_args` | …`.parameters` (the successor field) |
/// | `ruleset` (non-empty) | …`.variables["ruleset"]` |
/// | `path` | `tool_plugins.global_paths["cimp-audit@1/<id>"]` |
///
/// Three decisions worth stating rather than leaving to be reverse-engineered:
///
/// * **`path` goes to the machine-wide map, not a per-project one.** It was
///   already machine scope before v34 (the load/save write-through pair existed
///   precisely to keep it out of project overlays), so the machine-wide map is
///   where it already lived — moving it into a project map would *narrow* it.
/// * **An empty `ruleset` writes nothing.** Empty meant "use the tool's own
///   default", and in the container the absence of a value is exactly that,
///   while a stored `""` would render `--config ""` on the next scan with no way
///   back short of hand-editing the file.
/// * **`enabled` is always written**, even when it equals the manifest's
///   default. The manifest default can change between releases; a user who
///   accepted today's default did not thereby agree to tomorrow's, and this is
///   the one moment their actual selection is known.
///
/// The old array is REMOVED, because leaving it would leave two answers to
/// "is gitleaks on?" in one file, and the one nothing reads would be the one a
/// person editing by hand would find first.
///
/// Idempotent: a second pass finds `schema_version == 34` so `looks_v33` is
/// false, and the array it would read is gone in any case.
fn migrate_v33_to_v34(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    let legacy = root
        .get_mut("code_audit")
        .and_then(Value::as_object_mut)
        .and_then(|c| c.remove("tools"))
        .unwrap_or(Value::Null);

    if let Some(entries) = legacy.as_array() {
        let mut states = serde_json::Map::new();
        let mut paths = serde_json::Map::new();
        for e in entries {
            let Some(id) = e.get("id").and_then(Value::as_str) else {
                continue;
            };
            // An id this build never shipped (a hand edit, or a tool a future
            // version removed) is dropped, exactly as the pre-v34 lenient
            // deserializer dropped it. Migrating it would manufacture container
            // state for a tool that does not exist.
            if !V34_AUDIT_TOOL_IDS.contains(&id) {
                continue;
            }
            let mut state = serde_json::Map::new();
            state.insert(
                "enabled".to_string(),
                Value::Bool(e.get("enabled").and_then(Value::as_bool).unwrap_or(true)),
            );
            if let Some(t) = e.get("timeout_secs").and_then(Value::as_u64) {
                state.insert(
                    "timeout_secs".to_string(),
                    Value::Number(serde_json::Number::from(t)),
                );
            }
            if let Some(a) = e.get("extra_args").and_then(Value::as_array) {
                if !a.is_empty() {
                    state.insert("parameters".to_string(), Value::Array(a.clone()));
                }
            }
            if let Some(r) = e.get("ruleset").and_then(Value::as_str) {
                if !r.trim().is_empty() {
                    let mut vars = serde_json::Map::new();
                    vars.insert("ruleset".to_string(), Value::String(r.to_string()));
                    state.insert("variables".to_string(), Value::Object(vars));
                }
            }
            states.insert(id.to_string(), Value::Object(state));

            if let Some(path) = e.get("path").and_then(Value::as_str) {
                if !path.trim().is_empty() {
                    paths.insert(
                        format!("{V34_AUDIT_PLUGIN_KEY}/{id}"),
                        Value::String(path.to_string()),
                    );
                }
            }
        }

        if !states.is_empty() || !paths.is_empty() {
            // Merged into whatever `tool_plugins` already holds rather than
            // written over it: the v31 → v33 step created the container empty,
            // but a file that has been through a newer build once may already
            // carry user plugins, and losing those to an audit migration would
            // be the exact failure this step exists to prevent.
            let container = root
                .entry("tool_plugins".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !container.is_object() {
                *container = Value::Object(serde_json::Map::new());
            }
            let container = container.as_object_mut().expect("just made an object");

            if !states.is_empty() {
                let plugins = container
                    .entry("plugins".to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(plugins) = plugins.as_object_mut() {
                    // Phase E gate, B-E1: merge into the audit plugin's slot
                    // rather than writing over it, for the same reason the
                    // container itself is merged one level up. A slot that is
                    // already there was written by a build that reads the
                    // container, so it is NEWER than the array being moved —
                    // clobbering it would replace live configuration with a
                    // copy of the legacy shape. `or_insert` throughout: the
                    // stored value wins, per tool, exactly as `global_paths`
                    // below already does.
                    let slot = plugins
                        .entry(V34_AUDIT_PLUGIN_KEY.to_string())
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if !slot.is_object() {
                        *slot = Value::Object(serde_json::Map::new());
                    }
                    let slot = slot.as_object_mut().expect("just made an object");
                    slot.entry("enabled".to_string())
                        .or_insert(Value::Bool(true));
                    let tools = slot
                        .entry("tools".to_string())
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if !tools.is_object() {
                        *tools = Value::Object(serde_json::Map::new());
                    }
                    if let Some(tools) = tools.as_object_mut() {
                        for (k, v) in states {
                            tools.entry(k).or_insert(v);
                        }
                    }
                }
            }
            if !paths.is_empty() {
                let global = container
                    .entry("global_paths".to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(global) = global.as_object_mut() {
                    for (k, v) in paths {
                        global.entry(k).or_insert(v);
                    }
                }
            }
        }
    }

    // Stamps a *literal* 34 (not `CURRENT_SCHEMA_VERSION`): the v34 → v35 step
    // runs next in the same cascade pass and gates on `schema_version == 34`.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(34u8)),
    );
}

fn looks_v34(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 34)
}

fn migrate_v34_to_v35_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v34_to_v35(value)
}

/// Every per-tab injection-override cell, by its wire key.
///
/// The list mirrors `injection::TabInjectionOverrides`'s fields — i.e. exactly
/// `Feature::has_tab_scope`. A *frozen* copy on purpose: a migration step is a
/// statement about a file format at one moment, and rebuilding it from the live
/// feature table would make this step migrate a different set the day a control
/// is added, silently changing what a v34 file becomes. Adding a control later
/// needs its own step, or none at all — an absent cell still reads `inherit`.
const TAB_INJECTION_CELLS_V35: &[&str] = &[
    "taint_latch",
    "spotlighting",
    "detection",
    "ssrf_guard",
    "fetch_budgets",
    "memory_quarantine",
    "native_web",
    "consumer_hygiene",
    "tool_steering",
    "opencode_native_gate",
];

/// V34 → V35: **freeze every existing AI tab's injection posture in place**
/// before the per-tab default changes underneath it.
///
/// # What changed above this step
///
/// V39's posture decision moves the per-tab injection row from "inherit
/// everything" to "everything explicitly `Off`" for a **newly created** tab
/// (`injection::TabInjectionOverrides::all_off`), because the master and every
/// app-wide sub-protection now ship on and the per-tab row is the switch the
/// user actually reaches for (from the tab's shield badge).
///
/// # Why a file already on disk cannot be left alone
///
/// A cell absent from a settings file deserializes through `#[serde(default)]`
/// to `Override::Inherit`, and `Override::default()` deliberately stays
/// `Inherit`. So an untouched v34 file would keep resolving at L2 — correct
/// today. But "absent means inherit" is then the *only* thing standing between
/// an upgraded install and a silent posture change, and it is a property of two
/// defaults that a future edit could move without noticing this file. Writing
/// the word makes the file say what it means: **no silent posture change on
/// upgrade**, stated in the data rather than implied by a serde attribute.
///
/// # Exactly what it writes
///
/// For every `kind: "ai_tool"` tab: create `injection_overrides` if missing, and
/// insert `"inherit"` for each cell of [`TAB_INJECTION_CELLS_V35`] **that is not
/// already present**. A stored `"on"`, `"off"` — or even a hand-edited junk
/// value, which the resolver reads post-hoc as `inherit` (#48, G-1) — is left
/// byte-for-byte untouched: the user's own writes are not this step's business,
/// and rewriting a junk cell to `"inherit"` would erase the evidence of a typo
/// the user may want to find.
///
/// Non-AI tabs are untouched: they have no such field, and inventing one would
/// make a shell tab carry a row nothing reads.
///
/// Idempotent twice over: a second pass finds `schema_version == 35` so
/// [`looks_v34`] is false, and even run directly the cell-level insert is
/// `or_insert`-shaped, so nothing already written moves.
fn migrate_v34_to_v35(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            let Some(obj) = tab.as_object_mut() else {
                continue;
            };
            if obj.get("kind").and_then(Value::as_str) != Some("ai_tool") {
                continue;
            }
            let cells = obj
                .entry("injection_overrides".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            // A hand-edited non-object here (`"injection_overrides": null`) is
            // left alone rather than replaced: the typed load already reads any
            // non-object shape as an all-`Inherit` row, and replacing it would
            // be this step editing a value the user wrote.
            let Some(cells) = cells.as_object_mut() else {
                continue;
            };
            for key in TAB_INJECTION_CELLS_V35 {
                cells
                    .entry((*key).to_string())
                    .or_insert_with(|| Value::String("inherit".to_string()));
            }
        }
    }

    // Stamps a *literal* 35 (not `CURRENT_SCHEMA_VERSION`): the v35 → v36 step
    // runs next in the same cascade pass and gates on `schema_version == 35`.
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(35u8)),
    );
}

fn looks_v35(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 35)
}

fn migrate_v35_to_v36_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v35_to_v36(value)
}

fn looks_v36(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 36)
}

fn migrate_v36_to_v37_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v36_to_v37(value)
}

fn looks_v37(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 37)
}

fn migrate_v37_to_v38_step(value: &mut Value, _shell: &ShellSpec) {
    migrate_v37_to_v38(value)
}

/// The v36 schema's serialised defaults for the two lane fields — what a file
/// that never opened the colour picker carried (see [`migrate_v36_to_v37`]).
const V36_USAGE_COLOR_SESSION: &str = "#30363d";
const V36_USAGE_COLOR_AGENT: &str = "#3b6ea5";

/// v36 -> v37: the two fixed usage-lane colors become a **map keyed by the
/// harness's declared `TurnOrigin` id** (V40 Phase I, issue #107 item 4).
///
/// | v36 | v37 |
/// |---|---|
/// | `graph.usage_color_session` | `graph.usage_lane_colors["session"]` |
/// | `graph.usage_color_agent` | `graph.usage_lane_colors["agent"]` |
///
/// `session` and `agent` are the two lane ids both shipped harnesses declare,
/// so the keys are the ids the user's colors were already about — the pair was
/// simply spelled into core's schema instead of read off the declaration. A
/// harness with a third lane could not be given a color at all; now every lane
/// falls back to the palette slot for its declared position, and this map holds
/// only what the user actually picked.
///
/// Absent or non-string values are dropped rather than defaulted: an absent
/// entry means "use the declared position's palette slot", which is the answer
/// a fresh install gets and the right answer for a file that never set one.
///
/// **A value equal to the v36 default is dropped too** (rc.9 live-verify item
/// 38). The v36 fields were plain always-serialised strings, so EVERY v36 file
/// carried `#30363d` / `#3b6ea5` whether or not the user ever opened the
/// picker — copying those would pin every upgrading install to the v36 palette
/// and make a future palette change invisible to everyone but new users, the
/// exact mistake the test below names. A user who picked the default colour on
/// purpose loses nothing: the palette slot for that position IS that colour.
fn migrate_v36_to_v37(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(graph) = root.get_mut("graph").and_then(Value::as_object_mut) {
        let mut lanes = serde_json::Map::new();
        for (field, lane, v36_default) in [
            ("usage_color_session", "session", V36_USAGE_COLOR_SESSION),
            ("usage_color_agent", "agent", V36_USAGE_COLOR_AGENT),
        ] {
            if let Some(v) = graph.remove(field) {
                if v.as_str().is_some_and(|s| s != v36_default) {
                    lanes.insert(lane.to_string(), v);
                }
            }
        }
        // Merge rather than replace, for the same reason the v35 step merges
        // its harness rows: a hand-written `usage_lane_colors` is the user's
        // and outranks a field this step is retiring.
        if !lanes.is_empty() {
            match graph.get_mut("usage_lane_colors").and_then(Value::as_object_mut) {
                Some(existing) => {
                    for (k, v) in lanes {
                        existing.entry(k).or_insert(v);
                    }
                }
                None => {
                    graph.insert("usage_lane_colors".to_string(), Value::Object(lanes));
                }
            }
        }
    }
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(37u8)),
    );
}

/// The four AI notification slots, and the SUFFIX each one's seeded text ends
/// with.
///
/// A frozen list, like every other constant in this module (locked decision
/// 14). It describes the prose `default_ai_tab` wrote into files up to schema
/// 37 — "<name> is idle" and its three siblings — and rebuilding it from
/// today's seed would change what a v37 file becomes the day the wording does.
const V37_AI_NOTIFICATION_SUFFIXES: &[(&str, &str)] = &[
    ("idle", " is idle"),
    ("awaiting_permission", " is awaiting permission"),
    ("question", " has a question"),
    ("error", " encountered an error"),
];

/// v37 → v38: **the seeded AI notification prose becomes the `{tab}`
/// placeholder.**
///
/// Up to schema 37 a freshly seeded AI tab got its own name baked into all four
/// notification texts ("Claude is idle"). That went stale the moment the tab was
/// renamed, and it was simply wrong for a duplicate: `create_ai_tab` clones the
/// source tab's whole config, so "Claude 2" announced "Claude is idle".
/// `notifications::manager` now resolves `{tab}` to the tab's LIVE display name
/// at speak time, so this step moves existing files onto the placeholder.
///
/// **The rewrite rule, and why it is not "compare against this tab's name".**
/// For each of the four slots on each `ai_tool` tab: if the text ENDS WITH that
/// slot's seeded suffix and the prefix is non-empty and contains no `{`, the
/// text is replaced by `"{tab}<suffix>"`. Anything else is left exactly as the
/// user typed it.
///
/// Matching on the suffix rather than on `format!("{name}{suffix}")` is
/// deliberate: the stored `name` is the tab's name TODAY, and the text carries
/// the name it had when it was seeded. A tab duplicated from `claude` is named
/// "Claude 2" and carries "Claude is idle"; a renamed tab is named "Backend" and
/// carries "Claude is idle". Both are seeded prose that must move, and neither
/// matches its own name. Nor is a registry lookup enough — the source tab could
/// have been renamed before the duplicate was made, so the baked prefix may be
/// a string no registry ever contained.
///
/// The two guards are what keep a user-edited text safe:
/// * **non-empty prefix** — "" and " is idle" alone are not seeded prose.
/// * **no `{`** — a text already carrying `{tab}` (or any other placeholder) is
///   this build's own output or the user's, and re-writing it would be a
///   second, lossy pass. This is also what makes the step idempotent, which the
///   frozen-cascade guarantee needs: a v1 file reaching here has already been
///   handed the placeholder by the v1 → v2 step's embedded default.
///
/// **Only `ai_tool` tabs.** A Shell tab's seeded error text is
/// "Shell encountered an error", which ends with the same suffix; the kind check
/// is the only thing standing between that and a rewrite. A tab entry carrying
/// no `kind` at all (possible in a sparse project overlay) is skipped for the
/// same reason — leaving a text alone is always the recoverable direction.
fn migrate_v37_to_v38(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            let Some(obj) = tab.as_object_mut() else {
                continue;
            };
            if obj.get("kind").and_then(Value::as_str) != Some("ai_tool") {
                continue;
            }
            let Some(notifs) = obj.get_mut("notifications").and_then(Value::as_object_mut) else {
                continue;
            };
            for (field, suffix) in V37_AI_NOTIFICATION_SUFFIXES {
                // v1.11 promoted every slot to `{ enabled, text }`, so a v37
                // file always carries objects here; a bare string is a shape
                // this step does not know and does not touch.
                let Some(slot) = notifs.get_mut(*field).and_then(Value::as_object_mut) else {
                    continue;
                };
                let Some(text) = slot.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let Some(prefix) = text.strip_suffix(suffix) else {
                    continue;
                };
                if prefix.is_empty() || prefix.contains('{') {
                    continue;
                }
                slot.insert("text".to_string(), Value::String(format!("{{tab}}{suffix}")));
            }
        }
    }
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(38u8)),
    );
}

/// The two harness ids the v35 file format had FIELDS for.
///
/// A frozen list, like every other constant in this module (locked decision
/// 14). It describes the shape of files written before V40 Phase B, and a v35
/// file cannot have carried a field for a harness that did not exist when it
/// was written — so rebuilding it from the live registry would make this step
/// look for `codex_last_seen` in files that never had one, and would change
/// what a v35 file becomes the day a harness is added.
const HARNESSES_V36: &[&str] = &["claude", "opencode"];

/// v35 → v36 (V40 Phase B, locked decisions 5 and 6): **the per-harness
/// settings map**.
///
/// Every `*_claude` / `*_opencode` field PAIR in the v35 format becomes one
/// `harness.<id>` row, and every setting only one harness read becomes an `ext`
/// key on that row:
///
/// | v35 | v36 |
/// |---|---|
/// | `tool_plugins.expose_commands_{claude,opencode}` | `harness.<id>.expose_commands` |
/// | `code_audit.expose_{claude,opencode}` | `harness.<id>.expose_code_audit` |
/// | `harness_versions.{claude,opencode}_last_seen` | `harness.<id>.last_seen` |
/// | `harness_versions.claude_last_verified` | `harness.claude.last_verified` |
/// | `harness_versions.claude_auto_verify` | `harness.claude.auto_verify` |
/// | `harness_versions.input_profile_status` | `harness.<id>.input_profile_status` (**copied to every row**) |
/// | `offload.mcp_servers[].{claude,opencode}_access` | `…[].access.<id>.enabled` |
/// | `statusline.enabled` | `harness.claude.ext["statusline"]` |
/// | `claude_local.{base_url,auth_token,model_alias}` | `harness.claude.ext["local.*"]` |
/// | `offload.opencode_provider{,_auto}` | `harness.opencode.ext["provider"{,"_auto"}]` |
/// | `offload.injection.opencode_native_gate_enabled` | `harness.opencode.ext["native_gate"]` |
///
/// Three properties this step deliberately has:
///
/// * **Absent stays absent.** A key the file does not carry is not written —
///   the typed load resolves it from `HarnessSettings::defaults_for`, which is
///   what makes a harness added LATER need no migration at all. Backfilling
///   defaults here would be this step deciding what a future harness's default
///   is, at the moment it happens to run.
/// * **`input_profile_status` is copied to every row, not moved to one.** It
///   was a single scalar for all harnesses and the recorded spike was run
///   against whichever harnesses the user actually had, so copying is the
///   honest carry-over; moving it to one row would silently reset the other to
///   `"unverified"` and turn delegation off for it after an upgrade.
/// * **Nothing is deleted that a `#[serde(default)]` container would not
///   ignore anyway.** The old keys ARE removed, so the file stops carrying two
///   copies of the same fact; a value that fails to read as the expected type
///   is skipped rather than coerced, leaving the default in place.
fn migrate_v35_to_v36(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    // ── the core per-harness block ─────────────────────────────────────────
    let mut rows: serde_json::Map<String, Value> = serde_json::Map::new();
    for id in HARNESSES_V36 {
        rows.insert((*id).to_string(), Value::Object(serde_json::Map::new()));
    }

    fn put(rows: &mut serde_json::Map<String, Value>, id: &str, key: &str, v: Value) {
        if let Some(row) = rows.get_mut(id).and_then(Value::as_object_mut) {
            row.insert(key.to_string(), v);
        }
    }

    // `expose_commands_*` out of `tool_plugins`.
    if let Some(tp) = root.get_mut("tool_plugins").and_then(Value::as_object_mut) {
        for id in HARNESSES_V36 {
            if let Some(v) = tp.remove(&format!("expose_commands_{id}")) {
                if v.is_boolean() {
                    put(&mut rows, id, "expose_commands", v);
                }
            }
        }
    }
    // `expose_*` out of `code_audit` (`expose_offload` stays — the offload
    // worker is not a harness).
    if let Some(ca) = root.get_mut("code_audit").and_then(Value::as_object_mut) {
        for id in HARNESSES_V36 {
            if let Some(v) = ca.remove(&format!("expose_{id}")) {
                if v.is_boolean() {
                    put(&mut rows, id, "expose_code_audit", v);
                }
            }
        }
    }
    // The version/verify block.
    if let Some(hv) = root.get_mut("harness_versions").and_then(Value::as_object_mut) {
        for id in HARNESSES_V36 {
            if let Some(v) = hv.remove(&format!("{id}_last_seen")) {
                if v.is_string() {
                    put(&mut rows, id, "last_seen", v);
                }
            }
            if let Some(v) = hv.remove(&format!("{id}_last_verified")) {
                if v.is_string() {
                    put(&mut rows, id, "last_verified", v);
                }
            }
            if let Some(v) = hv.remove(&format!("{id}_auto_verify")) {
                if !v.is_null() {
                    put(&mut rows, id, "auto_verify", v);
                }
            }
        }
        // ONE scalar, copied to EVERY row — see the doc comment.
        if let Some(v) = hv.remove("input_profile_status") {
            if v.is_string() {
                for id in HARNESSES_V36 {
                    put(&mut rows, id, "input_profile_status", v.clone());
                }
            }
        }
    }

    // ── the plugin `ext` blocks ────────────────────────────────────────────
    let mut ext: serde_json::Map<String, Value> = serde_json::Map::new();
    for id in HARNESSES_V36 {
        ext.insert((*id).to_string(), Value::Object(serde_json::Map::new()));
    }
    fn put_ext(ext: &mut serde_json::Map<String, Value>, id: &str, key: &str, v: Value) {
        if let Some(row) = ext.get_mut(id).and_then(Value::as_object_mut) {
            row.insert(key.to_string(), v);
        }
    }

    if let Some(sl) = root.remove("statusline") {
        if let Some(enabled) = sl.get("enabled").filter(|v| v.is_boolean()) {
            put_ext(&mut ext, "claude", "statusline", enabled.clone());
        }
    }
    if let Some(cl) = root.remove("claude_local") {
        for field in ["base_url", "auth_token", "model_alias"] {
            if let Some(v) = cl.get(field).filter(|v| v.is_string()) {
                put_ext(&mut ext, "claude", &format!("local.{field}"), v.clone());
            }
        }
    }
    if let Some(off) = root.get_mut("offload").and_then(Value::as_object_mut) {
        if let Some(v) = off.remove("opencode_provider") {
            if v.is_object() || v.is_null() {
                put_ext(&mut ext, "opencode", "provider", v);
            }
        }
        if let Some(v) = off.remove("opencode_provider_auto") {
            if v.is_boolean() {
                put_ext(&mut ext, "opencode", "provider_auto", v);
            }
        }
        if let Some(inj) = off.get_mut("injection").and_then(Value::as_object_mut) {
            if let Some(v) = inj.remove("opencode_native_gate_enabled") {
                if v.is_boolean() {
                    put_ext(&mut ext, "opencode", "native_gate", v);
                }
            }
        }
        // ── per-server access pair → `access` map ──────────────────────────
        if let Some(servers) = off.get_mut("mcp_servers").and_then(Value::as_array_mut) {
            for server in servers.iter_mut() {
                let Some(obj) = server.as_object_mut() else {
                    continue;
                };
                let mut access = serde_json::Map::new();
                for id in HARNESSES_V36 {
                    if let Some(v) = obj.remove(&format!("{id}_access")) {
                        if let Some(on) = v.as_bool() {
                            access.insert(
                                (*id).to_string(),
                                serde_json::json!({ "enabled": on }),
                            );
                        }
                    }
                }
                if !access.is_empty() {
                    obj.insert("access".to_string(), Value::Object(access));
                }
            }
        }
    }

    for (id, block) in ext {
        if let Some(obj) = block.as_object() {
            if obj.is_empty() {
                continue;
            }
        }
        put(&mut rows, &id, "ext", block);
    }

    // Merge into whatever `harness` block the file already had — a hand-written
    // one, or a `harness.codex` key from a newer build the user downgraded
    // from. Existing keys WIN: a value the user (or a newer cImp) already put
    // in the new shape must not be overwritten by a stale copy of the old one.
    let existing = root
        .remove("harness")
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default();
    let mut merged = serde_json::Map::new();
    for (id, row) in rows {
        let Some(row_obj) = row.as_object() else {
            continue;
        };
        if row_obj.is_empty() && !existing.contains_key(&id) {
            // Nothing carried over and nothing there: leave the key absent so
            // the typed load supplies the declared defaults.
            continue;
        }
        let mut out = row_obj.clone();
        if let Some(Value::Object(prior)) = existing.get(&id) {
            for (k, v) in prior {
                // `ext` is a CONTAINER, not a value (V40 review finding M-5).
                // "Existing keys win" is right at the row level and wrong one
                // level down: a partial `ext` — a hand-written
                // `{"statusline": true}`, or one written by a newer build the
                // user downgraded from — would replace the whole block and
                // discard every key this step had just carried over, so a
                // migrated `local.base_url` silently reverted to
                // `http://localhost:4000` and the tab connected to a proxy the
                // user never configured. Merged per key instead, with prior
                // still winning on a collision.
                if let ("ext", Some(Value::Object(carried))) = (k.as_str(), out.get_mut(k)) {
                    if let Value::Object(prior_ext) = v {
                        for (ek, ev) in prior_ext {
                            carried.insert(ek.clone(), ev.clone());
                        }
                        continue;
                    }
                }
                out.insert(k.clone(), v.clone());
            }
        }
        merged.insert(id, Value::Object(out));
    }
    // Rows for harnesses this step knows nothing about ride through untouched.
    for (id, v) in existing {
        merged.entry(id).or_insert(v);
    }
    if !merged.is_empty() {
        root.insert("harness".to_string(), Value::Object(merged));
    }

    // Final cascade step ⇒ stamp CURRENT (36).
    root.insert(
        "schema_version".to_string(),
        Value::Number(serde_json::Number::from(36u8)),
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

/// Move a settings file aside, verbatim, to `<name>.<infix>.<unix-secs>.bak`.
///
/// The shared mechanism behind [`quarantine_corrupt_file`] and
/// [`quarantine_outdated_file`] — the two reasons a settings file gets set aside
/// are different enough to need different words for the user, and identical in
/// what has to happen to the bytes. Best-effort: a failed rename falls back to
/// copy+remove (cross-volume rename fails on Windows when, e.g., the launch_cwd
/// lives on a different drive than the user temp).
///
/// Returns `None` when there was no file to move, otherwise the target it aimed
/// at and whether the move actually happened. The caller does the logging, so
/// each reason keeps its own wording; a `Some((_, false))` caller must NOT go on
/// to overwrite `path`, because the original is still sitting there.
fn move_settings_file_aside(path: &Path, infix: &str) -> Option<(PathBuf, bool)> {
    if !path.exists() {
        return None;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = backup_path_for(path, &format!("{infix}.{ts}.bak"));
    let renamed = fs::rename(path, &target).is_ok();
    let moved = if renamed {
        true
    } else if fs::copy(path, &target).is_ok() {
        let _ = fs::remove_file(path);
        true
    } else {
        false
    };
    Some((target, moved))
}

/// Move a corrupt settings file aside before resetting to defaults. A total
/// failure just logs and returns `None` — the caller still resets to defaults.
///
/// "Corrupt" means the bytes did not parse. For a file that parsed perfectly
/// well and is merely older than this build can migrate, use
/// [`quarantine_outdated_file`] instead: telling a user their intact settings
/// were corrupt sends them looking for the wrong problem.
pub fn quarantine_corrupt_file(path: &Path) -> Option<PathBuf> {
    let (target, moved) = move_settings_file_aside(path, "corrupted")?;
    if moved {
        tracing::warn!(
            quarantine = %target.display(),
            "settings: corrupt file moved aside; defaults will be written"
        );
        Some(target)
    } else {
        tracing::warn!(
            path = %path.display(),
            target = %target.display(),
            "settings: could not quarantine corrupt file"
        );
        None
    }
}

/// Move a **valid but below-floor** settings file aside — one written by a cImp
/// older than [`MIN_GLOBAL_SCHEMA_VERSION`], whose migration steps this build no
/// longer carries. Same mechanism as [`quarantine_corrupt_file`], different name
/// on disk (`.outdated.` rather than `.corrupted.`) and different words in the
/// log, because it is a different thing that happened to the user.
///
/// Returns the quarantine path so the caller can name it in the error the user
/// actually sees; `None` if the file could not be moved, which the caller must
/// treat as "do not overwrite it".
pub fn quarantine_outdated_file(path: &Path) -> Option<PathBuf> {
    let (target, moved) = move_settings_file_aside(path, "outdated")?;
    if moved {
        tracing::warn!(
            quarantine = %target.display(),
            "settings: outdated file moved aside intact; defaults will be written"
        );
        Some(target)
    } else {
        tracing::warn!(
            path = %path.display(),
            target = %target.display(),
            "settings: could not move the outdated file aside; leaving it in place"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn fake_default_shell() -> ShellSpec {
        ShellSpec {
            command: PathBuf::from("/bin/bash"),
            args: vec!["-i".to_string()],
        }
    }

    /// A scratch directory that removes itself, so a floor test can write a real
    /// file and then look at what is beside it.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cimp_{tag}_{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        /// The `.bak` siblings of `settings.json`, by file name.
        fn baks(&self) -> Vec<String> {
            let mut out: Vec<String> = fs::read_dir(&self.0)
                .expect("read scratch dir")
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".bak"))
                .collect();
            out.sort();
            out
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // ── The migration floor (V42 R9, issue #120) ───────────────────────────

    /// **What counts as below the floor.** The `None` row is the one that needed
    /// deciding: `schema_version` did not exist before the v1.9 → v1.10 step, so
    /// a file that states none is a pre-v1.10 file — the case the deleted
    /// `looks_v1…` presence detectors used to recognise — and not a fresh
    /// install, which has no file at all and is seeded long before this is
    /// asked.
    #[test]
    fn the_global_floor_covers_old_stamps_and_no_stamp_at_all() {
        assert!(below_global_floor(&json!({ "schema_version": 20 })));
        assert!(below_global_floor(&json!({
            "schema_version": MIN_GLOBAL_SCHEMA_VERSION - 1
        })));
        // Pre-v1.10: no stamp to read. Below the floor, not "absent".
        assert!(below_global_floor(&json!({ "claude_code": { "command": "claude" } })));
        assert!(below_global_floor(&json!({})));
        // A stamp that is not a number is not a stamp.
        assert!(below_global_floor(&json!({ "schema_version": "30" })));

        assert!(!below_global_floor(&json!({
            "schema_version": MIN_GLOBAL_SCHEMA_VERSION
        })));
        assert!(!below_global_floor(&json!({
            "schema_version": crate::settings::schema::CURRENT_SCHEMA_VERSION
        })));
    }

    /// **The fixpoint guard must not force-stamp a below-floor file.**
    ///
    /// This is the whole hazard in one test. The guard at the bottom of
    /// `migrate_if_needed` exists to stop a re-migrate loop, and it does that by
    /// writing `CURRENT` over whatever the cascade left. Run it on a file whose
    /// steps no longer exist and it converts "old file we can still recognise"
    /// into "current file with everything defaulted" — permanently, because the
    /// stamp is the only evidence left. So the refusal happens first, and it
    /// leaves the file's own version, its own contents, and no backup behind.
    #[test]
    fn the_fixpoint_guard_does_not_stamp_a_below_floor_file() {
        let dir = TempDir::new("floor_stamp");
        let path = dir.join("settings.json");
        let shell = fake_default_shell();

        for original in [
            json!({ "schema_version": 20, "tabs": [], "offload": {} }),
            // No stamp at all — the pre-v1.10 shape.
            json!({ "claude_code": { "command": "claude" } }),
        ] {
            let mut v = original.clone();
            fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

            assert!(
                !migrate_if_needed(&mut v, &path, &shell).unwrap(),
                "a below-floor file reports no change, so the caller does not write it back"
            );
            assert_eq!(
                v, original,
                "not migrated and NOT force-stamped: the version on disk is the only thing that \
                 can still identify this file as old"
            );
            assert!(
                dir.baks().is_empty(),
                "no backup either — nothing was transformed to need one: {:?}",
                dir.baks()
            );
            fs::remove_file(&path).unwrap();
        }
    }

    /// The other side of the same guard: a file **at** the floor is ordinary
    /// work. It cascades, it lands on `CURRENT`, and it gets its one backup.
    #[test]
    fn a_file_at_the_floor_still_migrates_normally() {
        let dir = TempDir::new("floor_ok");
        let path = dir.join("settings.json");
        let shell = fake_default_shell();
        let mut v = json!({
            "schema_version": MIN_GLOBAL_SCHEMA_VERSION,
            "tabs": [],
            "offload": {},
        });
        fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

        assert!(migrate_if_needed(&mut v, &path, &shell).unwrap());
        assert_eq!(
            v["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION),
            "a file at the floor reaches the current schema — the floor retires steps, it does \
             not stop the ladder"
        );
        assert_eq!(
            dir.baks(),
            vec![format!("settings.json.v{MIN_GLOBAL_SCHEMA_VERSION}.bak")],
            "one backup, labelled with the version the user actually had"
        );
    }

    /// **Quarantine preserves the original bytes.** The floor's promise to the
    /// user is that their settings were set aside, not read and not deleted — so
    /// the quarantined file must be byte-identical to what was on disk,
    /// whitespace, key order, comments-that-are-not-comments and all.
    #[test]
    fn quarantining_an_outdated_file_preserves_it_byte_for_byte() {
        let dir = TempDir::new("floor_bytes");
        let path = dir.join("settings.json");
        // Deliberately not what `serde_json` would emit: odd spacing, a key
        // order nothing would reproduce, a trailing newline.
        let original = b"{\r\n  \"tabs\":   [],\n\t\"schema_version\":20 }\n".to_vec();
        fs::write(&path, &original).unwrap();

        let target = quarantine_outdated_file(&path).expect("the file is moved aside");
        assert!(!path.exists(), "the old file is no longer at the live path");
        assert_eq!(
            fs::read(&target).unwrap(),
            original,
            "the user's file must survive the floor unchanged — quarantine sets aside, it never \
             rewrites and never deletes"
        );
        assert_eq!(
            dir.baks().len(),
            1,
            "exactly one quarantine file: {:?}",
            dir.baks()
        );
        assert!(
            target
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".outdated."),
            "the name says WHY, and 'outdated' is not 'corrupted': {}",
            target.display()
        );
    }

    /// The two quarantine reasons share one mechanism and nothing else. A
    /// corrupt file and an outdated one must not land on the same name, or the
    /// only durable record of which happened is gone.
    #[test]
    fn corrupt_and_outdated_quarantines_do_not_share_a_name() {
        let dir = TempDir::new("floor_names");
        let path = dir.join("settings.json");

        fs::write(&path, b"{ not json").unwrap();
        let corrupt = quarantine_corrupt_file(&path).expect("moved aside");
        fs::write(&path, br#"{"schema_version": 20}"#).unwrap();
        let outdated = quarantine_outdated_file(&path).expect("moved aside");

        assert_ne!(corrupt, outdated);
        assert!(corrupt.to_string_lossy().contains(".corrupted."));
        assert!(outdated.to_string_lossy().contains(".outdated."));
        // Neither is there to be quarantined twice.
        assert_eq!(quarantine_corrupt_file(&path), None);
        assert_eq!(quarantine_outdated_file(&path), None);
    }

    /// V33 Phase E's step is a pure stamp: it must advance the marker and touch
    /// **nothing else**. A v30 file's LAN endpoints keep whatever auth they had
    /// (none), because the new fields default to `""` = no header.
    #[test]
    fn v30_to_v31_only_stamps_the_version() {
        let mut v = json!({
            "schema_version": 30,
            "graph": { "embedding_endpoint": "http://172.21.1.11:12344" },
            "offload": {
                "backends": [{ "kind": { "type": "local", "server_command": "llama-server" } }],
                "mcp_servers": [{ "name": "ddg", "url": "http://172.21.1.11:17201/mcp" }],
            },
        });
        let before = v.clone();
        migrate_v30_to_v31(&mut v);
        assert_eq!(v["schema_version"], json!(31));
        assert!(!looks_v30(&v));
        // Everything but the marker is byte-identical.
        let mut stripped = v.clone();
        stripped["schema_version"] = json!(30);
        assert_eq!(stripped, before);
        // Idempotent.
        let once = v.clone();
        migrate_v30_to_v31(&mut v);
        assert_eq!(v, once);
    }

    /// V37 Phase A's step is a pure stamp: it must advance the marker and touch
    /// **nothing else**. The C2 invariant — an existing config's effective tool
    /// surface is unchanged — is carried entirely by serde defaults, so any
    /// data written here would be a bug.
    #[test]
    fn v31_to_v32_only_stamps_the_version() {
        let mut v = json!({
            "schema_version": 31,
            "offload": {
                "mcp_servers": [
                    { "name": "ddg", "url": "http://172.21.1.11:17201/mcp", "claude_access": true },
                    { "name": "git", "command": "uvx", "args": ["mcp-server-git"] },
                ],
            },
        });
        let before = v.clone();
        migrate_v31_to_v32(&mut v);
        assert_eq!(v["schema_version"], json!(32));
        assert!(!looks_v31(&v));
        // No registry keys are written: the defaults do the work.
        assert!(v["offload"].get("mcp_categories").is_none());
        assert!(v["offload"].get("mcp_activation").is_none());
        assert!(v["offload"]["mcp_servers"][0].get("enabled").is_none());
        assert!(v["offload"]["mcp_servers"][0].get("origin").is_none());
        // Everything but the marker is byte-identical.
        let mut stripped = v.clone();
        stripped["schema_version"] = json!(31);
        assert_eq!(stripped, before);
        // Idempotent.
        let once = v.clone();
        migrate_v31_to_v32(&mut v);
        assert_eq!(v, once);
    }

    /// The C2 invariant itself, end to end: a v31 file run through the cascade
    /// deserializes to a `Settings` whose every MCP server is effectively
    /// enabled — same surface as before the upgrade, with no categories and no
    /// activation overrides.
    #[test]
    fn v31_to_v32_leaves_the_effective_mcp_surface_unchanged() {
        let shell = fake_default_shell();
        let mut v = json!({
            "schema_version": 31,
            "tabs": [],
            "offload": {
                "mcp_servers": [
                    { "name": "ddg", "url": "http://x/mcp", "claude_access": true },
                    { "name": "git", "command": "uvx", "offload_access": false },
                ],
            },
        });
        for step in MIGRATION_STEPS {
            if (step.detect)(&v) {
                (step.transform)(&mut v, &shell);
            }
        }
        let s: crate::settings::Settings = serde_json::from_value(v).unwrap();
        assert!(s.offload.mcp_categories.is_empty());
        assert!(s.offload.mcp_activation.categories.is_empty());
        assert!(s.offload.mcp_activation.servers.is_empty());
        for m in &s.offload.mcp_servers {
            assert!(m.enabled, "{} lost its surface", m.name);
            assert_eq!(m.origin, crate::settings::McpOrigin::External);
            assert!(crate::offload::mcp_host::server_enabled(
                m,
                &s.offload.mcp_categories,
                &s.offload.mcp_activation,
            ));
        }
        // The pre-existing access flags are untouched by the step.
        assert!(s.offload.mcp_servers[0]
            .access
            .get("claude")
            .is_some_and(|a| a.enabled));
        assert!(!s.offload.mcp_servers[1].offload_access);
    }

    // ── V40 Phase I: lane colours (issue #107 item 4) ─────────────────────

    /// **The user's two picked lane colours survive becoming a map** (schema
    /// 36 -> 37).
    ///
    /// The one thing a settings migration has to get right is that the user's
    /// existing answers survive it. `session` and `agent` are the lane ids both
    /// shipped harnesses declare, so the keys are what the two retired fields
    /// were already about — the pair was spelled into core's schema instead of
    /// read off the declaration.
    #[test]
    fn v36_to_v37_moves_the_lane_pair_into_the_map() {
        let mut v = json!({
            "schema_version": 36,
            "graph": {
                "usage_color_session": "#112233",
                "usage_color_agent": "#445566",
                "usage_color_in": "#58a6ff",
            },
        });
        migrate_v36_to_v37(&mut v);
        assert_eq!(v["schema_version"], json!(37));
        assert_eq!(
            v["graph"]["usage_lane_colors"],
            json!({ "session": "#112233", "agent": "#445566" })
        );
        assert!(v["graph"].get("usage_color_session").is_none());
        assert!(v["graph"].get("usage_color_agent").is_none());
        // The KIND colours are cImp's own pricing vocabulary, not a lane —
        // they stay exactly where they are.
        assert_eq!(v["graph"]["usage_color_in"], json!("#58a6ff"));
        // The result deserializes, which is the half a shape assertion misses.
        let mut full = serde_json::to_value(crate::settings::Settings::default()).unwrap();
        full["graph"] = v["graph"].clone();
        let typed: crate::settings::Settings = serde_json::from_value(full).unwrap();
        assert_eq!(
            typed.graph.usage_lane_colors.get("agent").map(String::as_str),
            Some("#445566")
        );
    }

    /// **A file that never picked a colour gets no row.**
    ///
    /// Absent means "the palette slot for this lane's declared position", which
    /// is the answer a fresh install gets. Writing the old defaults in as
    /// explicit rows would pin every existing install to today's palette and
    /// make a future palette change invisible to everyone but new users — the
    /// "empty is not absent" mistake in the other direction.
    /// **The v36 defaults are not a pick.** Every v36 file carried them (the
    /// fields were always serialised), so copying them would pin every
    /// upgrading install to that palette — rc.9 live-verify item 38 found
    /// exactly this on a real profile. One default + one real pick ⇒ only the
    /// pick survives.
    #[test]
    fn v36_to_v37_drops_lane_values_equal_to_the_v36_defaults() {
        let mut v = json!({
            "schema_version": 36,
            "graph": {
                "usage_color_session": "#30363d",
                "usage_color_agent": "#3b6ea5",
            },
        });
        migrate_v36_to_v37(&mut v);
        assert!(v["graph"].get("usage_lane_colors").is_none());
        assert!(v["graph"].get("usage_color_session").is_none());
        assert!(v["graph"].get("usage_color_agent").is_none());

        let mut mixed = json!({
            "schema_version": 36,
            "graph": {
                "usage_color_session": "#30363d",
                "usage_color_agent": "#445566",
            },
        });
        migrate_v36_to_v37(&mut mixed);
        assert_eq!(mixed["graph"]["usage_lane_colors"], json!({ "agent": "#445566" }));
    }

    #[test]
    fn v36_to_v37_writes_no_lane_map_for_a_file_that_carried_nothing() {
        let mut v = json!({ "schema_version": 36, "graph": { "usage_color_in": "#58a6ff" } });
        migrate_v36_to_v37(&mut v);
        assert!(v["graph"].get("usage_lane_colors").is_none());
        // …and a file with no `graph` block at all is untouched but stamped.
        let mut bare = json!({ "schema_version": 36 });
        migrate_v36_to_v37(&mut bare);
        assert_eq!(bare, json!({ "schema_version": 37 }));
    }

    // --- v37 → v38 ----------------------------------------------------------

    /// The seeded prose moves onto the placeholder — including on a tab whose
    /// NAME no longer matches what was baked into it, which is the case that
    /// made "Claude 2" announce "Claude is idle".
    #[test]
    fn v37_to_v38_rewrites_seeded_notification_prose_to_the_tab_placeholder() {
        let mut v = json!({
            "schema_version": 37,
            "tabs": [
                {
                    "kind": "ai_tool",
                    "id": "claude",
                    "name": "Claude",
                    "notifications": {
                        "idle": { "enabled": true, "text": "Claude is idle" },
                        "awaiting_permission": { "enabled": true, "text": "Claude is awaiting permission" },
                        "question": { "enabled": true, "text": "Claude has a question" },
                        "error": { "enabled": false, "text": "Claude encountered an error" }
                    }
                },
                {
                    // Duplicated from `claude`, then the source was renamed:
                    // neither this tab's name nor any registry name matches the
                    // baked prefix. The suffix rule still catches it.
                    "kind": "ai_tool",
                    "id": "ai-7f3c",
                    "name": "Claude 2",
                    "notifications": {
                        "idle": { "enabled": true, "text": "Backend work is idle" },
                        "awaiting_permission": { "enabled": true, "text": "OpenCode is awaiting permission" },
                        "question": { "enabled": true, "text": "Claude (custom provider) has a question" },
                        "error": { "enabled": true, "text": "Zeta encountered an error" }
                    }
                }
            ]
        });
        assert!(looks_v37(&v));
        migrate_v37_to_v38(&mut v);
        let tabs = v["tabs"].as_array().unwrap();
        for t in tabs {
            let n = &t["notifications"];
            assert_eq!(n["idle"]["text"], "{tab} is idle");
            assert_eq!(n["awaiting_permission"]["text"], "{tab} is awaiting permission");
            assert_eq!(n["question"]["text"], "{tab} has a question");
            assert_eq!(n["error"]["text"], "{tab} encountered an error");
        }
        // `enabled` is the user's and is not touched by a prose rewrite.
        assert_eq!(tabs[0]["notifications"]["error"]["enabled"], json!(false));
        assert_eq!(v["schema_version"], json!(38));
        assert!(!looks_v37(&v));
    }

    /// A user-edited text is left exactly as typed, and so is a Shell tab whose
    /// seeded error prose ends with the very same suffix.
    #[test]
    fn v37_to_v38_leaves_edited_texts_and_shell_tabs_alone() {
        let mut v = json!({
            "schema_version": 37,
            "tabs": [
                {
                    "kind": "ai_tool",
                    "id": "claude",
                    "name": "Claude",
                    "notifications": {
                        "idle": { "enabled": true, "text": "hey, your build finished" },
                        "awaiting_permission": { "enabled": true, "text": " is awaiting permission" },
                        "question": { "enabled": true, "text": "{tab} has a question" },
                        "error": { "enabled": true, "text": "" }
                    }
                },
                {
                    "kind": "shell",
                    "id": "shell-1",
                    "name": "Shell 1",
                    "notifications": {
                        "error": { "enabled": true, "text": "Shell encountered an error" },
                        "exited": { "enabled": true, "text": "Shell exited (code {code})" }
                    }
                },
                // No `kind` (a sparse project overlay): skipped, not guessed at.
                {
                    "id": "ai-9",
                    "notifications": { "idle": { "enabled": true, "text": "Nine is idle" } }
                }
            ]
        });
        migrate_v37_to_v38(&mut v);
        let tabs = v["tabs"].as_array().unwrap();
        let ai = &tabs[0]["notifications"];
        assert_eq!(ai["idle"]["text"], "hey, your build finished");
        assert_eq!(
            ai["awaiting_permission"]["text"], " is awaiting permission",
            "an empty prefix is not seeded prose"
        );
        assert_eq!(
            ai["question"]["text"], "{tab} has a question",
            "already-migrated text is left alone \u{2014} the step is idempotent"
        );
        assert_eq!(ai["error"]["text"], "");
        assert_eq!(tabs[1]["notifications"]["error"]["text"], "Shell encountered an error");
        assert_eq!(tabs[2]["notifications"]["idle"]["text"], "Nine is idle");
    }

    /// Running the step twice must change nothing the second time — the
    /// cascade re-enters at whatever version a file is stamped with, and a
    /// frozen step has to survive being reached from every entry point.
    #[test]
    fn v37_to_v38_is_idempotent() {
        let seed = json!({
            "schema_version": 37,
            "tabs": [{
                "kind": "ai_tool", "id": "claude", "name": "Claude",
                "notifications": { "idle": { "enabled": true, "text": "Claude is idle" } }
            }]
        });
        let mut once = seed.clone();
        migrate_v37_to_v38(&mut once);
        let mut twice = once.clone();
        twice["schema_version"] = json!(37);
        migrate_v37_to_v38(&mut twice);
        assert_eq!(once, twice);
        // …and a file with no `tabs` at all is untouched but stamped.
        let mut bare = json!({ "schema_version": 37 });
        migrate_v37_to_v38(&mut bare);
        assert_eq!(bare, json!({ "schema_version": 38 }));
    }

    /// As a CASCADE member: a file entering well below still lands on the
    /// current version with its seeded prose on the placeholder.
    #[test]
    fn the_cascade_rewrites_notification_prose_on_the_way_to_the_current_version() {
        let shell = fake_default_shell();
        let mut v = json!({
            "schema_version": 33,
            "tabs": [{
                "kind": "ai_tool", "id": "claude", "name": "Claude", "command": "claude",
                "notifications": {
                    "idle": { "enabled": true, "text": "Claude is idle" },
                    "awaiting_permission": { "enabled": true, "text": "Claude is awaiting permission" },
                    "question": { "enabled": true, "text": "Claude has a question" },
                    "error": { "enabled": true, "text": "Claude encountered an error" }
                }
            }],
            "offload": {},
        });
        for step in MIGRATION_STEPS {
            if (step.detect)(&v) {
                (step.transform)(&mut v, &shell);
            }
        }
        assert_eq!(
            v["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION)
        );
        assert_eq!(v["tabs"][0]["notifications"]["idle"]["text"], "{tab} is idle");
    }

    /// **The first two palette slots are the colours the retired settings
    /// defaulted to** (V40 Phase I, issue #107 item 4).
    ///
    /// The acceptance for item 4 is that a user who never picked a colour sees
    /// no change: lane 0 stays `#30363d` and lane 1 stays `#3b6ea5`. Those
    /// values moved from `GraphSettings`'s defaults into a palette array in
    /// `CodeIntelligenceView.svelte`, where no Rust test would otherwise see
    /// them — so this reads the array out of the component, the same way
    /// `every_settings_reader_runs_the_harness_parse_boundary` reads a function
    /// body out of its own file.
    ///
    /// Newline-agnostic: CI checks this tree out with CRLF.
    #[test]
    fn the_first_two_lane_palette_slots_are_the_shipped_colours() {
        let src = include_str!("../../../src/lib/CodeIntelligenceView.svelte").replace('\r', "");
        let at = src
            .find("const LANE_PALETTE = [")
            .expect("`LANE_PALETTE` is gone — re-point this test");
        let rest = &src[at..];
        let body = &rest[rest.find('[').expect("the array opens") + 1
            ..rest.find(']').expect("the array closes")];
        let slots: Vec<&str> = body
            .split(',')
            .map(|s| s.trim().trim_matches('\'').trim_matches('"'))
            .filter(|s| !s.is_empty())
            .collect();
        assert!(
            slots.len() >= 4,
            "the lane palette has {} slots — a harness with four lanes would fall through to the \
             overflow colour before the reader could tell them apart: {slots:?}",
            slots.len()
        );
        assert_eq!(
            &slots[..2],
            ["#30363d", "#3b6ea5"],
            "slots 0 and 1 are the colours `usage_color_session` / `usage_color_agent` defaulted \
             to; changing them recolours every existing install's usage donut, which item 4 \
             promised it would not"
        );
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for s in &slots {
            assert!(
                s.len() == 7 && s.starts_with('#'),
                "{s:?} is not a `#rrggbb` literal"
            );
            assert!(seen.insert(s), "duplicate palette slot {s} — two lanes would paint alike");
        }
    }

    // ── V40 Phase I: the PROJECT OVERLAY cascade (issue #107 item 5) ───────

    /// **A stale project overlay is migrated, and the `claude_*` pair lands in
    /// `harness.<id>`** — the gap item 5 names.
    ///
    /// A project that set `claude_local.base_url` and `statusline.enabled`
    /// before schema 36 kept them as top-level keys in `.cimp/config.json`. The
    /// GLOBAL file's v35 -> v36 step moved those fields under `harness.claude`,
    /// the overlay was never migrated, and the project's values then reached
    /// nothing — with the file still on disk saying otherwise. Silent, and
    /// exactly the "empty is not absent" shape: the merged settings were not
    /// missing a value, they were carrying the harness default while a file
    /// two directories away stated a different one.
    #[test]
    fn a_v35_overlay_migrates_its_claude_pair_into_the_harness_map() {
        let shell = fake_default_shell();
        let mut overlay = json!({
            "claude_local": { "base_url": "http://myproxy:9000" },
            "statusline": { "enabled": false },
            "ui": { "theme": "future-light" },
        });
        assert!(migrate_overlay(&mut overlay, 35, &shell), "the overlay changed");
        assert_eq!(
            overlay["harness"]["claude"]["ext"]["local.base_url"],
            json!("http://myproxy:9000")
        );
        assert_eq!(overlay["harness"]["claude"]["ext"]["statusline"], json!(false));
        // The v35 spellings are GONE — leaving them would re-merge a key the
        // current schema does not read, which is how the value went stale in
        // the first place.
        assert!(overlay.get("claude_local").is_none());
        assert!(overlay.get("statusline").is_none());
        // Everything the step does not own is untouched, and the overlay stays
        // SPARSE: a cascade that stamped whole-object defaults here would
        // override the global baseline through `deep_merge`, which is the
        // silent data loss that kept the overlay unmigrated until now.
        assert_eq!(overlay["ui"], json!({ "theme": "future-light" }));
        let keys: Vec<&str> = overlay
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["ui", "harness"]);
    }

    /// **The entry stamp never survives into the merge.**
    ///
    /// `save` writes `schema_version` into the overlay so `load` knows which
    /// schema the project's keys are in. It is an entry marker for
    /// `migrate_overlay` and nothing else: left in place it would `deep_merge`
    /// over the global's own `schema_version` and pin the merged `Settings`
    /// below whatever the global file actually reached.
    #[test]
    fn the_overlay_entry_stamp_is_always_stripped() {
        let shell = fake_default_shell();
        let current = crate::settings::schema::CURRENT_SCHEMA_VERSION as u64;
        // Current-schema overlay: nothing to do but drop the stamp.
        let mut cur = json!({ "schema_version": current, "ui": { "theme": "tui" } });
        assert!(migrate_overlay(&mut cur, current, &shell));
        assert_eq!(cur, json!({ "ui": { "theme": "tui" } }));
        // Stale overlay: migrated AND unstamped.
        let mut old = json!({ "schema_version": 35, "statusline": { "enabled": true } });
        assert!(migrate_overlay(&mut old, 35, &shell));
        assert!(old.get("schema_version").is_none());
        assert_eq!(old["harness"]["claude"]["ext"]["statusline"], json!(true));
        // Below the floor: still unstamped, and otherwise untouched.
        let mut ancient = json!({ "schema_version": 9, "tabs": [] });
        migrate_overlay(&mut ancient, 9, &shell);
        assert_eq!(ancient, json!({ "tabs": [] }));
    }

    /// **The cascade refuses to start below the migration floor.**
    ///
    /// Until V42 R9 this test was about the pre-`schema_version` era, where the
    /// detectors keyed off ABSENT top-level keys that every sparse overlay lacks
    /// by construction. Those rows are gone; the floor now means the simpler
    /// thing, that there are no rows down there at all. The claim is unchanged:
    /// an overlay too old to place is left exactly as it was.
    ///
    /// The second half is what stops this from being a test about a no-op. The
    /// SAME value entered one version higher — at the floor — is rewritten, and
    /// rewritten in the way that motivated migrating overlays at all: a project
    /// that set `claude_local.base_url` before schema 36 kept a top-level
    /// `claude_local` block whose value reached nothing after the global file
    /// moved the field into the harness map.
    #[test]
    fn the_overlay_cascade_refuses_below_the_migration_floor() {
        let shell = fake_default_shell();
        let before = json!({ "claude_local": { "base_url": "http://box:8080" } });

        let mut v = before.clone();
        assert!(!migrate_overlay(
            &mut v,
            MIN_OVERLAY_SCHEMA_VERSION - 1,
            &shell
        ));
        assert_eq!(v, before, "an overlay too old to place must be left alone");

        let mut at_the_floor = before.clone();
        assert!(migrate_overlay(
            &mut at_the_floor,
            MIN_OVERLAY_SCHEMA_VERSION,
            &shell
        ));
        assert_eq!(
            at_the_floor.pointer("/harness/claude/ext/local.base_url"),
            Some(&json!("http://box:8080")),
            "one version higher the same overlay IS rewritten — so the refusal above is a \
             decision, not a description of a cascade that does nothing"
        );
    }

    /// **A current-shape overlay is not touched.**
    ///
    /// The ordinary case, and the one the old skip was protecting: a sparse
    /// diff written by this build must come out of `migrate_overlay` byte for
    /// byte, whatever keys it happens to lack.
    #[test]
    fn a_current_overlay_passes_through_unchanged() {
        let shell = fake_default_shell();
        let current = crate::settings::schema::CURRENT_SCHEMA_VERSION as u64;
        let before = json!({
            "checks_allow_remote_worker": true,
            "harness": { "claude": { "ext": { "statusline": false } } },
            "tabs": [],
        });
        let mut v = before.clone();
        assert!(!migrate_overlay(&mut v, current, &shell));
        assert_eq!(v, before);
    }

    /// **v35 → v36: every field pair lands in its harness's row** (V40 locked
    /// decision 5).
    ///
    /// The one thing a settings migration has to get right is that the user's
    /// EXISTING answers survive it. Live-verify 1 and 10 are the human version
    /// of this; this is the mechanical one, driven over a file carrying every
    /// pair the step moves, with values chosen so a swapped Claude/OpenCode
    /// copy would be visible rather than symmetric.
    #[test]
    fn v35_to_v36_copies_every_field_pair_into_the_harness_map() {
        let mut v = json!({
            "schema_version": 35,
            "statusline": { "enabled": false },
            "claude_local": {
                "base_url": "http://localhost:9999",
                "auth_token": "sk-house",
                "model_alias": "local-opus",
            },
            "tool_plugins": {
                "expose_commands_claude": false,
                "expose_commands_opencode": true,
                "plugins": {},
            },
            "code_audit": {
                "expose_claude": true,
                "expose_opencode": false,
                "expose_offload": true,
            },
            "harness_versions": {
                "claude_last_seen": "2.1.232",
                "claude_last_verified": "2.1.14",
                "opencode_last_seen": "1.18.13",
                "claude_auto_verify": { "version": "2.1.232", "at_ms": 7, "status": "fail",
                                        "failures": [] },
                "input_profile_status": "pass",
                "e1_status": "pass",
                "d0_status": "unverified",
            },
            "offload": {
                "opencode_provider_auto": true,
                "opencode_provider": { "base_url": "http://127.0.0.1:8080/v1", "model": "Q",
                                       "api_key": "", "source_command": "llama-server" },
                "injection": { "opencode_native_gate_enabled": false },
                "mcp_servers": [
                    { "name": "ddg", "claude_access": true, "opencode_access": false,
                      "offload_access": true },
                ],
            },
        });
        migrate_v35_to_v36(&mut v);
        assert_eq!(v["schema_version"], json!(36));

        let claude = &v["harness"]["claude"];
        let opencode = &v["harness"]["opencode"];

        // The three core pairs, each half in its own row and NOT swapped.
        assert_eq!(claude["expose_commands"], json!(false));
        assert_eq!(opencode["expose_commands"], json!(true));
        assert_eq!(claude["expose_code_audit"], json!(true));
        assert_eq!(opencode["expose_code_audit"], json!(false));
        assert_eq!(claude["last_seen"], json!("2.1.232"));
        assert_eq!(opencode["last_seen"], json!("1.18.13"));
        assert_eq!(claude["last_verified"], json!("2.1.14"));
        assert_eq!(claude["auto_verify"]["status"], json!("fail"));

        // The single spike scalar reaches EVERY row. Copied, not moved: the
        // recorded outcome was a human's judgement about whichever harnesses
        // the user had, and moving it to one row would reset the other to
        // `"unverified"` and switch its delegation off after an upgrade.
        assert_eq!(claude["input_profile_status"], json!("pass"));
        assert_eq!(opencode["input_profile_status"], json!("pass"));

        // The plugin `ext` blocks.
        assert_eq!(claude["ext"]["statusline"], json!(false));
        assert_eq!(claude["ext"]["local.base_url"], json!("http://localhost:9999"));
        assert_eq!(claude["ext"]["local.auth_token"], json!("sk-house"));
        assert_eq!(claude["ext"]["local.model_alias"], json!("local-opus"));
        assert_eq!(opencode["ext"]["native_gate"], json!(false));
        assert_eq!(opencode["ext"]["provider_auto"], json!(true));
        assert_eq!(opencode["ext"]["provider"]["model"], json!("Q"));

        // V40 review L-6: this step's `ext` key literals are FROZEN (locked
        // decision 14) and the plugins' `pub const`s are not, so nothing tied
        // the two together — renaming a const would orphan every migrated value
        // into an undeclared key `normalize_harness_settings` deliberately
        // leaves alone. Driven off THIS file, which sets every old field, the
        // two sets have to be equal in both directions.
        for d in crate::harness::registry::HARNESSES {
            let declared: std::collections::BTreeSet<&str> =
                d.plugin.settings_schema().iter().map(|f| f.key).collect();
            let migrated: std::collections::BTreeSet<&str> = v["harness"][d.id]["ext"]
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            assert_eq!(
                migrated, declared,
                "{}: the frozen 35 -> 36 step and `settings_schema()` must spell the same                  `ext` keys",
                d.id
            );
        }

        // The per-server access pair.
        let server = &v["offload"]["mcp_servers"][0];
        assert_eq!(server["access"]["claude"]["enabled"], json!(true));
        assert_eq!(server["access"]["opencode"]["enabled"], json!(false));
        assert_eq!(server["offload_access"], json!(true), "not a harness, stays put");

        // The old keys are GONE, so no file carries two copies of one fact.
        assert!(v.get("statusline").is_none());
        assert!(v.get("claude_local").is_none());
        assert!(v["tool_plugins"].get("expose_commands_claude").is_none());
        assert!(v["code_audit"].get("expose_claude").is_none());
        assert!(v["harness_versions"].get("claude_last_seen").is_none());
        assert!(v["harness_versions"].get("input_profile_status").is_none());
        assert!(v["offload"].get("opencode_provider").is_none());
        assert!(v["offload"]["injection"]
            .get("opencode_native_gate_enabled")
            .is_none());
        assert!(server.get("claude_access").is_none());
        // …but the two spike outcomes that stay ARE still there.
        assert_eq!(v["harness_versions"]["e1_status"], json!("pass"));

        // And the whole thing loads typed, which is the only claim that
        // matters: a step that produced JSON `Settings` cannot read would
        // quarantine the file.
        let typed: crate::settings::Settings =
            serde_json::from_value(v).expect("the migrated file loads typed");
        let claude_id = crate::harness::HarnessId::from_id("claude").expect("registered");
        assert_eq!(typed.harness_settings(claude_id).last_seen, "2.1.232");
        assert_eq!(
            typed.harness_ext(claude_id, "local.auth_token"),
            json!("sk-house")
        );
    }

    /// **A v35 file that carries NONE of the pairs gets no rows at all.**
    ///
    /// The absent case is what makes a later harness free: the step writes only
    /// what it found, and everything else resolves through
    /// `HarnessSettings::defaults_for` at load. Backfilling defaults here would
    /// be this step deciding a future harness's default at the moment it
    /// happened to run.
    #[test]
    fn v35_to_v36_writes_no_row_for_a_file_that_carried_nothing() {
        let mut v = json!({ "schema_version": 35, "tabs": [], "offload": {} });
        migrate_v35_to_v36(&mut v);
        assert_eq!(v["schema_version"], json!(36));
        assert!(
            v.get("harness").is_none(),
            "nothing to carry over ⇒ no `harness` block: {v}"
        );

        // …and the typed load still answers every declared default.
        let typed: crate::settings::Settings = serde_json::from_value(v).expect("loads");
        for h in crate::harness::registry::all() {
            assert!(typed.harness_settings(h).expose_commands, "{h}");
            assert_eq!(typed.harness_settings(h).input_profile_status, "unverified");
        }
    }

    /// **A `harness` block already present WINS, and an unknown id rides
    /// through.**
    ///
    /// Two cases in one, because both are about the same downgrade/upgrade
    /// hazard. A user who ran a newer build (or hand-wrote the new shape) has
    /// values in `harness.*` that must not be overwritten by a stale copy of
    /// the old field; and a `harness.codex` row this build knows nothing about
    /// must survive, or opening an older cImp once is a data-loss operation.
    #[test]
    fn v35_to_v36_keeps_an_existing_harness_block_and_an_unknown_id() {
        let mut v = json!({
            "schema_version": 35,
            "harness": {
                "claude": { "last_seen": "2.2.0" },
                "codex": { "last_seen": "0.9.1", "ext": { "sandbox_mode": "read-only" } },
            },
            "harness_versions": { "claude_last_seen": "1.0.0-stale" },
        });
        migrate_v35_to_v36(&mut v);
        assert_eq!(
            v["harness"]["claude"]["last_seen"], json!("2.2.0"),
            "the value already in the NEW shape wins over the old field"
        );
        assert_eq!(v["harness"]["codex"]["last_seen"], json!("0.9.1"));
        assert_eq!(
            v["harness"]["codex"]["ext"]["sandbox_mode"], json!("read-only"),
            "a row for a harness this build does not know must ride through"
        );

        // …and survives the typed round trip, which is where a `HarnessId` key
        // would have dropped it.
        let typed: crate::settings::Settings = serde_json::from_value(v).expect("loads");
        let back = serde_json::to_value(&typed).expect("re-serialize");
        assert_eq!(back["harness"]["codex"]["last_seen"], json!("0.9.1"));
        assert_eq!(
            back["harness"]["codex"]["ext"]["sandbox_mode"],
            json!("read-only")
        );
    }

    /// **A partial existing `ext` MERGES with the carried-over one** (V40 review
    /// finding M-5).
    ///
    /// "Existing keys win" is right at the ROW level and wrong one level down:
    /// `ext` is a container, so inserting the prior row's `"ext"` key replaced
    /// the whole block and discarded every key this step had just moved into it.
    /// A v35 file carrying both `harness.claude.ext = {"statusline": true}` (a
    /// hand edit, or a write by a newer build the user downgraded from) and
    /// `claude_local.base_url` lost the URL entirely, and the tab then connected
    /// to `http://localhost:4000` — a proxy the user never configured.
    #[test]
    fn v35_to_v36_merges_a_partial_existing_ext_instead_of_replacing_it() {
        let mut v = json!({
            "schema_version": 35,
            "harness": {
                "claude": { "ext": { "statusline": true, "local.model_alias": "mine" } },
            },
            "claude_local": {
                "base_url": "http://myproxy:9000",
                "auth_token": "sk-real",
                "model_alias": "stale-from-the-old-field",
            },
            "statusline": { "enabled": false },
        });
        migrate_v35_to_v36(&mut v);
        let ext = &v["harness"]["claude"]["ext"];
        assert_eq!(
            ext["local.base_url"], json!("http://myproxy:9000"),
            "a key carried over by this step must survive a partial existing `ext`"
        );
        assert_eq!(ext["local.auth_token"], json!("sk-real"));
        // …and prior still WINS per key, both for a key the step also carried
        // over and for one only the prior block had.
        assert_eq!(
            ext["local.model_alias"], json!("mine"),
            "the value already in the NEW shape wins over the old field"
        );
        assert_eq!(
            ext["statusline"], json!(true),
            "…including against `statusline.enabled: false`"
        );

        // The typed round trip keeps all four.
        let typed: crate::settings::Settings = serde_json::from_value(v).expect("loads");
        let row = typed.harness_settings(crate::harness::DEFAULT_HARNESS);
        assert_eq!(
            row.ext.get("local.base_url").and_then(|v| v.as_str()),
            Some("http://myproxy:9000")
        );
        assert_eq!(row.ext.get("statusline"), Some(&json!(true)));
    }

    /// V32 → V33 moves nothing: the V38 `tool_plugins` container is additive and
    /// `#[serde(default)]`, so an existing file is byte-identical apart from the
    /// marker. Phase E's v33 → v34 step is the one that moves `code_audit.tools`,
    /// and it gates on the version this stamps.
    #[test]
    fn v32_to_v33_only_stamps_the_version_and_v33_moves_the_audit_tools() {
        let mut v = json!({
            "schema_version": 32,
            "code_audit": {
                "enabled": true,
                "tools": [{ "id": "gitleaks", "enabled": true, "path": "C:\\bin\\gitleaks.exe" }],
            },
            "checks": [{ "name": "cargo", "cmd": "cargo check", "parser": "cargo-json" }],
        });
        let before = v.clone();
        migrate_v32_to_v33(&mut v);
        assert_eq!(v["schema_version"], json!(33));
        assert!(!looks_v32(&v));
        // Everything but the marker is byte-identical — `code_audit` in
        // particular is untouched (Phase E owns that move).
        let mut stripped = v.clone();
        stripped["schema_version"] = json!(32);
        assert_eq!(stripped, before);
        // Idempotent.
        let once = v.clone();
        migrate_v32_to_v33(&mut v);
        assert_eq!(v, once);

        // …and the NEXT step in the same cascade pass is the one that moves it.
        migrate_v33_to_v34(&mut v);
        assert_eq!(v["schema_version"], json!(34));
        assert!(v["code_audit"].get("tools").is_none(), "the array is removed");
        assert_eq!(
            v["tool_plugins"]["global_paths"]["cimp-audit@1/gitleaks"],
            json!("C:\\bin\\gitleaks.exe")
        );
    }

    /// **The v34 move, field by field**, against a settings file that has been
    /// configured the way a real one gets configured: some tools off, a path
    /// set, extra arguments, a ruleset override, a longer timeout.
    ///
    /// This is the step that decides whether an upgrading user keeps their audit
    /// configuration or silently gets a fresh one, so every mapping is asserted
    /// by name rather than by round-tripping a blob.
    #[test]
    fn v33_to_v34_moves_a_configured_audit_roster_into_the_container() {
        let mut v = json!({
            "schema_version": 33,
            "code_audit": {
                "enabled": true,
                "timeout_secs": 900,
                "quality_auto_select": false,
                "tools": [
                    {
                        "id": "gitleaks",
                        "enabled": true,
                        "path": "C:\\bin\\gitleaks.exe",
                        "extra_args": ["--redact"],
                        "ruleset": "",
                        "timeout_secs": null
                    },
                    {
                        "id": "semgrep",
                        "enabled": false,
                        "path": "",
                        "extra_args": [],
                        "ruleset": "p/ci",
                        "timeout_secs": 1200
                    },
                    {
                        "id": "dotnet-analyzers",
                        "enabled": true,
                        "path": "",
                        "extra_args": [],
                        "ruleset": "   ",
                        "timeout_secs": null
                    },
                    // A tool this build never shipped: dropped, exactly as the
                    // pre-v34 lenient deserializer dropped it. Migrating it
                    // would manufacture state for something that cannot run.
                    { "id": "guarddog", "enabled": true, "path": "C:\\bin\\gd.exe" }
                ]
            },
            // A user plugin the container already carries — an audit migration
            // must not cost the user their own plugins.
            "tool_plugins": {
                "plugins": { "acme@1.0.0": { "enabled": false, "tools": {} } },
                "global_paths": { "acme@1.0.0/scan": "D:\\acme.exe" }
            }
        });
        migrate_v33_to_v34(&mut v);
        assert_eq!(v["schema_version"], json!(34));

        // The umbrella-level settings STAY on `code_audit` — they are facts
        // about the feature, not about any one tool.
        assert_eq!(v["code_audit"]["enabled"], json!(true));
        assert_eq!(v["code_audit"]["timeout_secs"], json!(900));
        assert_eq!(v["code_audit"]["quality_auto_select"], json!(false));
        assert!(v["code_audit"].get("tools").is_none());

        let tools = &v["tool_plugins"]["plugins"]["cimp-audit@1"]["tools"];
        assert_eq!(v["tool_plugins"]["plugins"]["cimp-audit@1"]["enabled"], json!(true));

        // `enabled` is ALWAYS written, even when it equals what the manifest
        // would default to: the manifest default can change between releases,
        // and a user who accepted today's did not agree to tomorrow's.
        assert_eq!(tools["gitleaks"]["enabled"], json!(true));
        assert_eq!(tools["semgrep"]["enabled"], json!(false));
        // `extra_args` → `parameters`, the successor field.
        assert_eq!(tools["gitleaks"]["parameters"], json!(["--redact"]));
        assert!(tools["semgrep"].get("parameters").is_none(), "empty stays absent");
        // `timeout_secs` rides across; absent stays absent.
        assert_eq!(tools["semgrep"]["timeout_secs"], json!(1200));
        assert!(tools["gitleaks"].get("timeout_secs").is_none());
        // A non-empty `ruleset` becomes the declared variable's value…
        assert_eq!(tools["semgrep"]["variables"]["ruleset"], json!("p/ci"));
        // …and a blank one becomes NOTHING: blank meant "use the tool's own
        // default", which in the container is the absence of a value. Storing
        // `""` would render `--config ""` on the next scan with no way back.
        assert!(tools["gitleaks"].get("variables").is_none());
        assert!(tools["dotnet-analyzers"].get("variables").is_none());
        // A path is machine scope and always was, so it lands in the
        // machine-wide map rather than in a per-project one.
        assert_eq!(
            v["tool_plugins"]["global_paths"]["cimp-audit@1/gitleaks"],
            json!("C:\\bin\\gitleaks.exe")
        );
        assert!(
            v["tool_plugins"]["global_paths"]
                .get("cimp-audit@1/semgrep")
                .is_none(),
            "an empty path is not a path"
        );
        // The unknown id is dropped on both sides.
        assert!(tools.get("guarddog").is_none());
        assert!(v["tool_plugins"]["global_paths"].get("cimp-audit@1/guarddog").is_none());
        // The user's own plugin state survived untouched.
        assert_eq!(v["tool_plugins"]["plugins"]["acme@1.0.0"]["enabled"], json!(false));
        assert_eq!(v["tool_plugins"]["global_paths"]["acme@1.0.0/scan"], json!("D:\\acme.exe"));

        // Idempotent: a second pass has no array to read and stamps the same.
        let once = v.clone();
        migrate_v33_to_v34(&mut v);
        assert_eq!(v, once);
    }

    /// A v33 file with NO audit tools configured must not gain a container
    /// entry: "the user never touched this" and "the user set it to the
    /// defaults" are different states, and only the first lets a later release
    /// change a default.
    #[test]
    fn v33_to_v34_writes_nothing_when_there_was_nothing_to_move() {
        for tools in [json!([]), json!(null)] {
            let mut v = json!({
                "schema_version": 33,
                "code_audit": { "enabled": true, "tools": tools },
            });
            migrate_v33_to_v34(&mut v);
            assert_eq!(v["schema_version"], json!(34));
            assert!(
                v.get("tool_plugins").is_none(),
                "an empty roster must not manufacture container state"
            );
        }
        // …and a file with no `code_audit` block at all is just a version stamp.
        let mut v = json!({ "schema_version": 33 });
        migrate_v33_to_v34(&mut v);
        assert_eq!(v, json!({ "schema_version": 34 }));
    }

    /// The ids this frozen step knows are the ids the shipped roster uses.
    ///
    /// The step deliberately carries its own list (R4: a migration describes a
    /// file shape that existed on one day, and must keep describing it), so this
    /// is the check that the list was RIGHT when it was written — a typo here
    /// would silently drop one tool's configuration for every upgrading user.
    #[test]
    fn the_v34_migration_ids_are_the_shipped_roster() {
        let shipped: std::collections::BTreeSet<String> = crate::plugins::builtin::plugin_set()
            .plugins
            .iter()
            .flat_map(|p| p.manifest.tools.iter())
            .map(|t| t.id.clone())
            .collect();
        let step: std::collections::BTreeSet<String> =
            V34_AUDIT_TOOL_IDS.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(
            step, shipped,
            "the v33 → v34 step migrates a different set of tool ids than cImp ships — a tool in \
             the roster but not the step loses every upgrading user's configuration for it"
        );
        assert_eq!(
            V34_AUDIT_PLUGIN_KEY,
            crate::plugins::builtin::AUDIT_PLUGIN_KEY,
            "the step writes container keys under a plugin key nothing reads"
        );
    }

    /// **Phase E gate, B-E1: the move MERGES into the audit plugin's slot.**
    ///
    /// The container is merged one level up already (a file that has been
    /// through a newer build may carry user plugins). One level down was still
    /// a clobber: a `cimp-audit@1` slot that is present was written by a build
    /// that READS the container, so it is newer than the array being moved —
    /// replacing it with a copy of the legacy shape would undo live
    /// configuration during an upgrade, which is the exact failure this step
    /// exists to prevent. Stored value wins per tool, `global_paths`-style.
    #[test]
    fn the_move_never_clobbers_container_state_a_newer_build_wrote() {
        let mut v = json!({
            "schema_version": 33,
            "tool_plugins": { "plugins": { "cimp-audit@1": {
                "enabled": false,
                "tools": { "semgrep": { "enabled": true, "parameters": ["--new"] } }
            } } },
            "code_audit": { "tools": [
                { "id": "semgrep", "enabled": false, "extra_args": ["--legacy"] },
                { "id": "gitleaks", "enabled": false }
            ] },
        });
        migrate_v33_to_v34(&mut v);

        let slot = &v["tool_plugins"]["plugins"]["cimp-audit@1"];
        assert_eq!(slot["enabled"], json!(false), "the stored plugin flag wins");
        assert_eq!(
            slot["tools"]["semgrep"]["parameters"],
            json!(["--new"]),
            "the stored tool state wins over the legacy array"
        );
        assert_eq!(slot["tools"]["semgrep"]["enabled"], json!(true));
        // A tool the container had NO state for still arrives.
        assert_eq!(slot["tools"]["gitleaks"]["enabled"], json!(false));
    }

    /// **The v34 → v35 step: no silent posture change on upgrade.**
    ///
    /// V39 flips the per-tab injection default from "inherit everything" to
    /// "everything explicitly off" for a NEWLY CREATED tab. A file already on
    /// disk must keep the behaviour it had, so the step writes the word
    /// `inherit` into every cell that is absent and touches nothing else.
    ///
    /// Three properties, all of them the point of the step:
    /// absent ⇒ `inherit`; stored values untouched; idempotent.
    #[test]
    fn v34_to_v35_writes_inherit_into_absent_tab_cells_only() {
        let mut v = json!({
            "schema_version": 34,
            "tabs": [
                // An AI tab with no row at all — the common upgraded shape.
                { "kind": "ai_tool", "id": "claude", "command": "claude" },
                // An AI tab the user configured: one `on`, one `off`, one junk
                // value (which the resolver reads post-hoc as `inherit`, #48
                // G-1) — none of the three may move.
                { "kind": "ai_tool", "id": "opencode", "command": "opencode",
                  "injection_overrides": {
                      "taint_latch": "on",
                      "consumer_hygiene": "off",
                      "detection": true,
                  } },
                // Not an AI tab: it has no such field and must not gain one.
                { "kind": "shell", "id": "shell-1" },
            ],
        });
        migrate_v34_to_v35(&mut v);
        assert_eq!(v["schema_version"], json!(35));

        let claude = &v["tabs"][0]["injection_overrides"];
        for key in TAB_INJECTION_CELLS_V35 {
            assert_eq!(claude[*key], json!("inherit"), "{key} on the bare tab");
        }
        assert_eq!(
            claude.as_object().map(serde_json::Map::len),
            Some(TAB_INJECTION_CELLS_V35.len()),
            "the step writes the cells it declares and nothing else"
        );

        let opencode = &v["tabs"][1]["injection_overrides"];
        assert_eq!(opencode["taint_latch"], json!("on"), "a stored `on` stays");
        assert_eq!(opencode["consumer_hygiene"], json!("off"), "a stored `off` stays");
        assert_eq!(
            opencode["detection"],
            json!(true),
            "even a hand-edited junk cell stays — rewriting it would erase the evidence"
        );
        assert_eq!(opencode["spotlighting"], json!("inherit"), "the absent ones fill");

        assert!(
            v["tabs"][2].get("injection_overrides").is_none(),
            "a shell tab has no injection row"
        );

        // Idempotent run directly, not just via the version gate.
        let once = v.clone();
        migrate_v34_to_v35(&mut v);
        assert_eq!(v, once);
    }

    /// The behavioural half of the step, through the TYPED shape: a migrated
    /// legacy tab still resolves at L2, while a tab the app creates today does
    /// not. This is the property the JSON assertions above are a proxy for, and
    /// it is the one the locked decision is actually about.
    #[test]
    fn a_migrated_tab_keeps_inheriting_while_a_new_tab_ships_off() {
        let mut v = json!({
            "schema_version": 34,
            "tabs": [{ "kind": "ai_tool", "id": "claude", "command": "claude",
                       "name": "Claude" }],
        });
        migrate_v34_to_v35(&mut v);
        let migrated: crate::settings::Settings =
            serde_json::from_value(v).expect("the migrated shape deserializes");
        assert!(
            crate::settings::injection::effective(
                crate::settings::injection::Feature::TaintLatch,
                crate::settings::injection::Scope::tab_only("claude"),
                &migrated,
            ),
            "an upgraded tab keeps resolving at L2 — the whole point of the step"
        );
        assert!(!crate::settings::injection::protection_reduced(&migrated));

        // The tab the app creates today is the other half of the decision.
        let fresh = crate::settings::Settings {
            tabs: vec![crate::settings::schema::default_claude_tab()],
            ..Default::default()
        };
        assert!(!crate::settings::injection::effective(
            crate::settings::injection::Feature::TaintLatch,
            crate::settings::injection::Scope::tab_only("claude"),
            &fresh,
        ));
    }

    /// A file that entered the cascade far below still lands on 35 with its AI
    /// tabs filled — the step has to work as a CASCADE member, not only when
    /// called directly.
    #[test]
    fn the_cascade_fills_tab_cells_on_the_way_to_35() {
        let shell = fake_default_shell();
        let mut v = json!({
            "schema_version": 33,
            "tabs": [{ "kind": "ai_tool", "id": "claude", "command": "claude" }],
            "offload": {},
        });
        for step in MIGRATION_STEPS {
            if (step.detect)(&v) {
                (step.transform)(&mut v, &shell);
            }
        }
        assert_eq!(
            v["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION),
            "the cascade must reach the current version, not stop at whatever was current when              this test was written"
        );
        assert_eq!(v["tabs"][0]["injection_overrides"]["taint_latch"], json!("inherit"));
    }

    /// **The V37/V38 linearization caveat, pinned** (develop merge, 2026-08-19).
    ///
    /// Both milestones took `v31 → v32` while unreleased: V37 landed the MCP
    /// registry stamp on develop, V38 had taken the same number for its additive
    /// `tool_plugins` container and then `v32 → v33` for the audit move. The
    /// merge linearized V38's pair onto `v32 → v33` and `v33 → v34`, which means
    /// a DEV machine that ran a pre-merge V38 build holds a file stamped 32 or 33
    /// with a V38 meaning — a state no released build ever produced (real users
    /// are at v31 or below).
    ///
    /// No content-aware detector was built for it because none is needed, and
    /// this is the test that says why: the additive step only stamps, and the
    /// move step only moves `code_audit.tools` IF IT IS THERE. So both pre-merge
    /// shapes fall through the renumbered cascade onto the same result —
    /// the one still carrying the array gets it moved, the one that already
    /// moved it keeps the container it built. A file stamped 33 by the old build
    /// never runs V37's v31 → v32 step, which costs nothing: that step writes no
    /// data (serde defaults carry the C2 invariant).
    #[test]
    fn a_pre_merge_v38_dev_file_converges_on_the_current_shape() {
        let shell = fake_default_shell();
        let cascade = |mut v: Value| {
            for step in MIGRATION_STEPS {
                if (step.detect)(&v) {
                    (step.transform)(&mut v, &shell);
                }
            }
            v
        };

        // (a) Stamped 32 by the pre-merge V38 build: the container exists and is
        //     empty, the audit array has NOT moved yet.
        let a = cascade(json!({
            "schema_version": 32,
            "tabs": [],
            "offload": {},
            "tool_plugins": { "plugins": {}, "project_paths": {}, "global_paths": {} },
            "code_audit": { "tools": [{ "id": "semgrep", "enabled": false }] },
        }));
        assert_eq!(
            a["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION)
        );
        assert!(a["code_audit"].get("tools").is_none(), "the array still moves");
        assert_eq!(
            a["tool_plugins"]["plugins"]["cimp-audit@1"]["tools"]["semgrep"]["enabled"],
            json!(false)
        );

        // (b) Stamped 33 by the pre-merge V38 build: the move already happened.
        //     The renumbered move step must be a no-op over it, not a reset.
        let b = cascade(json!({
            "schema_version": 33,
            "tabs": [],
            "offload": {},
            "tool_plugins": {
                "plugins": { "cimp-audit@1": { "enabled": true, "tools": {
                    "semgrep": { "enabled": false, "parameters": ["--foo"] } } } },
                "project_paths": {},
                "global_paths": { "cimp-audit@1/semgrep": "C:/bin/semgrep.exe" },
            },
            "code_audit": { "enabled": true },
        }));
        assert_eq!(
            b["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION)
        );
        assert_eq!(
            b["tool_plugins"]["plugins"]["cimp-audit@1"]["tools"]["semgrep"]["parameters"],
            json!(["--foo"])
        );
        assert_eq!(
            b["tool_plugins"]["global_paths"]["cimp-audit@1/semgrep"],
            json!("C:/bin/semgrep.exe")
        );
        // Both land on a shape `Settings` accepts.
        for v in [a, b] {
            let s: crate::settings::Settings =
                serde_json::from_value(v).expect("the migrated shape deserializes");
            assert!(s.tool_plugins.plugins.contains_key("cimp-audit@1"));
        }
    }

    /// A v31 file loads with an empty container rather than failing — the
    /// "additive" claim, tested against the typed shape rather than trusted.
    #[test]
    fn a_v31_file_deserializes_with_an_empty_tool_plugins_container() {
        let v = json!({ "schema_version": 31 });
        let s: crate::settings::Settings =
            serde_json::from_value(v).expect("a v31 file must still deserialize");
        assert!(s.tool_plugins.plugins.is_empty());
        assert!(s.tool_plugins.global_paths.is_empty());
        assert!(s.tool_plugins.project_paths.is_empty());
    }

    /// **The ladder is unbroken from the floor to the top.**
    ///
    /// The cascade's last step must land exactly on `CURRENT_SCHEMA_VERSION` — a
    /// schema bump that forgets its `MIGRATION_STEPS` entry trips the
    /// `migrate_if_needed` fixpoint guard in production, and this test here.
    ///
    /// Rebased from v28 to the floor by V42 R9: a file entering at
    /// [`MIN_GLOBAL_SCHEMA_VERSION`] is the oldest one that exists as far as this
    /// build is concerned, so this is the longest cascade there is. A GAP in the
    /// ladder — a deletion that stopped one row short of the floor — shows up
    /// here as a file that stalls partway.
    #[test]
    fn cascade_from_the_floor_reaches_the_current_schema_version() {
        let shell = fake_default_shell();
        let mut v = json!({
            "schema_version": MIN_GLOBAL_SCHEMA_VERSION,
            "offload": {},
            "tabs": [],
        });
        for step in MIGRATION_STEPS {
            if (step.detect)(&v) {
                (step.transform)(&mut v, &shell);
            }
        }
        assert_eq!(
            v["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION),
        );
    }

    /// **The upgrade path, end to end, on a file that predates the container
    /// entirely.**
    ///
    /// The unit test above drives one step over a hand-built `Value`. This runs
    /// the whole cascade the way a launch does — every detector, every
    /// transform, in order — over the OLDEST file this build accepts (V42 R9
    /// rebased it from v29 to the floor), whose audit tools are configured, and
    /// then DESERIALIZES the result into `Settings`. That last part is the half a
    /// `Value`-level test cannot reach: a step can produce a shape that looks
    /// right and still not parse into the typed container the registry reads, and
    /// the user would meet that as "my scanners forgot everything" rather than as
    /// a failing test.
    #[test]
    fn an_old_file_with_configured_audit_tools_cascades_into_the_container() {
        let shell = fake_default_shell();
        let mut v = json!({
            "schema_version": MIN_GLOBAL_SCHEMA_VERSION,
            "offload": {},
            "tabs": [],
            "code_audit": {
                "enabled": true,
                "timeout_secs": 1800,
                "quality_auto_select": false,
                "tools": [
                    {
                        "id": "semgrep",
                        "enabled": false,
                        "path": "C:\\py\\Scripts\\semgrep.exe",
                        "extra_args": ["--exclude", "vendor"],
                        "ruleset": "p/ci",
                        "timeout_secs": 1200
                    },
                    { "id": "typos", "enabled": true, "path": "", "extra_args": [] }
                ]
            }
        });
        for step in MIGRATION_STEPS {
            if (step.detect)(&v) {
                (step.transform)(&mut v, &shell);
            }
        }
        assert_eq!(
            v["schema_version"],
            json!(crate::settings::schema::CURRENT_SCHEMA_VERSION)
        );

        let s: crate::settings::Settings =
            serde_json::from_value(v).expect("the migrated file must deserialize");

        // The umbrella settings survived the move.
        assert!(s.code_audit.enabled);
        assert_eq!(s.code_audit.timeout_secs, 1800);
        assert!(!s.code_audit.quality_auto_select);

        // …and every per-tool value landed where the registry looks for it.
        let plugin = &s.tool_plugins.plugins[crate::plugins::builtin::AUDIT_PLUGIN_KEY];
        assert!(plugin.enabled);
        let semgrep = &plugin.tools["semgrep"];
        assert!(!semgrep.enabled);
        assert_eq!(semgrep.timeout_secs, Some(1200));
        assert_eq!(semgrep.parameters, vec!["--exclude", "vendor"]);
        assert_eq!(semgrep.variables["ruleset"], "p/ci");
        assert!(plugin.tools["typos"].enabled);
        assert_eq!(
            s.tool_plugins.global_paths["cimp-audit@1/semgrep"],
            "C:\\py\\Scripts\\semgrep.exe"
        );

        // The migrated state is what the REGISTRY answers with — the join, not
        // just the storage. This is the assertion that would have caught a key
        // the container holds and the registry never looks under.
        let tools = crate::plugins::registry::effective_tools(
            &crate::plugins::builtin::plugin_set(),
            &s.tool_plugins,
            None,
        );
        let find = |id: &str| tools.iter().find(|t| t.tool_id == id).expect("built-in tool");
        let semgrep = find("semgrep");
        assert!(!semgrep.enabled, "the user had it switched off");
        assert_eq!(semgrep.path.as_deref(), Some("C:\\py\\Scripts\\semgrep.exe"));
        assert_eq!(semgrep.timeout_secs, Some(1200));
        assert_eq!(semgrep.variables["ruleset"], "p/ci");
        assert_eq!(semgrep.parameters, vec!["--exclude", "vendor"]);
        // A tool the file never mentioned keeps its manifest defaults, which is
        // how a fresh install and an upgraded one end up agreeing.
        let gitleaks = find("gitleaks");
        assert!(gitleaks.enabled && gitleaks.path.is_none());
        assert!(!find("dotnet-analyzers").enabled, "still opt-in");
    }

    /// Altitude tripwire: if `schema::LOCAL_DATA_TOOLS` ever gains a member,
    /// migrating a settings file whose cloud backend carries the historical
    /// "web/docs only" exclusion must still yield an exclusion list that denies
    /// EVERY current local-data tool. This fails the moment a new local-data tool
    /// is added without a matching migration step to backfill it (exactly the V21
    /// `list_dir` regression this guards) — forcing the author to add the
    /// migration rather than silently re-opening the hole.
    ///
    /// V42 R9 rebased it from the pre-v22 five-item fingerprint to the v30 one,
    /// because v30 is now the oldest file that exists: the v21 → v22 and
    /// v29 → v30 backfills are below the floor and deleted, and the seven names
    /// below are the frozen list the last of them wrote. They are spelled out
    /// rather than read from `schema::LOCAL_DATA_TOOLS` on purpose — a fixture
    /// that tracks the constant it is testing cannot fail.
    #[test]
    fn local_data_tools_growth_requires_a_backfilling_migration() {
        let shell = fake_default_shell();
        // The web-scope exclusion exactly as a v30 file carries it.
        let mut v = json!({
            "schema_version": MIN_GLOBAL_SCHEMA_VERSION,
            "offload": {
                "backends": [{
                    "name": "cloud",
                    "tool_scope": {
                        "mode": "allexcept",
                        "tools": [
                            "read_file", "list_dir", "code_search", "run_command",
                            "run_check", "filesystem", "git"
                        ]
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
                 preset with a FROZEN literal of its own, and widen the fixture above to match \
                 what that step writes.",
                crate::settings::schema::CURRENT_SCHEMA_VERSION - 1,
                crate::settings::schema::CURRENT_SCHEMA_VERSION,
            );
        }
    }

    /// **The backup is labelled with the version the user actually had, and the
    /// ladder has a row for every version between the floor and the top.**
    ///
    /// V0.6's contract was "the LOWEST matching detector wins", because the
    /// pre-`schema_version` detectors were presence archaeology and several could
    /// match one file at once. V42 R9 deleted those: every remaining detector is
    /// `schema_version == N`, so at most one can ever match and the entry label
    /// is simply the file's own stamp. Walking the whole range asserts the
    /// stronger property that replaces the old one — there is no version between
    /// the floor and current that `detect_entry_version` shrugs at, which is what
    /// a one-row gap in the ladder would look like.
    #[test]
    fn detect_entry_version_is_the_files_own_version_at_every_step() {
        let current = crate::settings::schema::CURRENT_SCHEMA_VERSION as u64;
        for version in MIN_GLOBAL_SCHEMA_VERSION..current {
            assert_eq!(
                detect_entry_version(&json!({ "schema_version": version })),
                Some(format!("v{version}").as_str()),
                "no step claims a v{version} file: either the ladder has a gap, or a step's \
                 `from_version` label disagrees with the version its detector matches"
            );
        }
        // A file already at the current schema skips the cascade…
        assert_eq!(
            detect_entry_version(&json!({ "schema_version": current })),
            None
        );
        // …and so does one below the floor, whose steps are gone. It is stopped
        // before this by `below_global_floor`; that it *also* matches nothing
        // here is why it would otherwise have reached the force-stamp silently.
        assert_eq!(
            detect_entry_version(&json!({ "schema_version": MIN_GLOBAL_SCHEMA_VERSION - 1 })),
            None
        );
        assert_eq!(detect_entry_version(&json!({})), None);
    }
}

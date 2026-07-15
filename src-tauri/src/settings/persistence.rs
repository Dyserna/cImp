//! JSON load/save with corruption recovery + version migrations.
//!
//! Two files participate:
//!
//!   * **Global** (`<exe-dir>/settings.json`) — portable baseline. Written
//!     once on first launch when missing; never rewritten through normal
//!     edits afterwards. Hand-edit to change defaults.
//!   * **Custom overlay** (`<launch_cwd>/.cimp/config.json`) — per
//!     launch-directory delta layered on top of global, kept inside the
//!     project's `.cimp` data dir alongside the code-graph `graph.db`.
//!     Created the first time a user customizes anything from a given
//!     working directory and deleted automatically when the diff is empty.
//!     A pre-consolidation overlay at the old loose path
//!     (`<launch_cwd>/.cimp.custom.config.json`) is migrated into `.cimp/`
//!     on the next launch (see `migrate_legacy_overlay`).
//!
//! On-disk format for both files is the same JSON object shape (matching
//! `Settings`). The custom file is allowed to be a *partial* object — any
//! keys it doesn't carry fall through to global. Older shapes are detected
//! by their discriminator fields and routed through the `migration`
//! module after the merge so a hand-imported old file at the new path
//! still upgrades cleanly. After migration an integrity check reconciles
//! the three reserved AI builtins (claude, claude-local, opencode)
//! with `enabled_ai_tabs`: every enabled id is forced present with
//! `builtin: true`, every reserved id absent from the list is dropped.
//! The `shell-default-1` reserved id is *not* re-seeded by the integrity
//! check: it ships on fresh installs only, and stays closed once a user
//! closes it.
//!
//! `load` always returns a usable `Settings` and a snapshot of the global
//! baseline (so the save path can compute diffs without re-reading disk).
//! Missing/corrupt files become defaults; the corrupt original is moved
//! aside as a `.bak`.

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};
use crate::settings::migration;
use crate::settings::write_atomic;
use crate::settings::schema::{
    default_ai_tab, default_audit_tools, default_code_audit_tab, default_code_quality_tab,
    default_graph_monitor_tab,
    default_graph_view_tab,
    default_offload_server_tab,
    default_shell_1_tab, default_tool_activity_tab, default_workbench_tab,
    starter_prompt_templates,
    AiTabId, HarnessVersions, LayoutNodePersisted, LlmPricingModel, PromptTemplate, Settings,
    TabConfig,
    CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, CODE_AUDIT_TAB_ID, CODE_QUALITY_TAB_ID,
    GRAPH_MONITOR_TAB_ID, GRAPH_VIEW_TAB_ID, OFFLOAD_SERVER_TAB_ID, OPENCODE_TAB_ID,
    SHELL_DEFAULT_TAB_ID, TOOL_ACTIVITY_TAB_ID, WORKBENCH_TAB_ID,
};
use crate::shell::ShellSpec;

const GLOBAL_FILE_NAME: &str = "settings.json";
/// The per-project cImp data directory. Holds the per-folder settings overlay
/// (`config.json`) and the code-graph store (`graph.db`); the home for any
/// future cImp-specific per-project file. Kept as a literal here rather than
/// tied to `graph.db_subdir`: the overlay determines where the overlay is read
/// from, so its location can't depend on a value that lives *inside* it.
const CIMP_DIR_NAME: &str = ".cimp";
/// Per-folder overlay filename, inside `<launch_cwd>/.cimp/`.
const CUSTOM_FILE_NAME: &str = "config.json";
/// Pre-consolidation overlay filename — a loose file in `launch_cwd`, migrated
/// into `.cimp/` on load.
const LEGACY_CUSTOM_FILE_NAME: &str = ".cimp.custom.config.json";

/// `<exe-dir>/settings.json` — the portable baseline. Falls back to the
/// current working directory if `current_exe()` is unavailable, which
/// shouldn't happen on any platform we ship to but is logged loudly if
/// it does.
pub fn global_path() -> AppResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::Settings(format!("current_exe failed: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| AppError::Settings("exe has no parent dir".into()))?;
    Ok(dir.join(GLOBAL_FILE_NAME))
}

/// `<launch_cwd>/.cimp/config.json` — the per-folder overlay, inside the
/// project's `.cimp` data dir (alongside `graph.db`).
pub fn custom_path(launch_cwd: &Path) -> PathBuf {
    launch_cwd.join(CIMP_DIR_NAME).join(CUSTOM_FILE_NAME)
}

/// `<launch_cwd>/.cimp.custom.config.json` — the pre-consolidation loose
/// overlay path. Read as a fallback and migrated into `.cimp/` on load.
fn legacy_custom_path(launch_cwd: &Path) -> PathBuf {
    launch_cwd.join(LEGACY_CUSTOM_FILE_NAME)
}

/// Path to *read* the overlay from: the canonical `.cimp/config.json` if it
/// exists, else the legacy loose file if that exists, else the canonical path
/// (the absent case). The side-effectful [`load`] first calls
/// [`migrate_legacy_overlay`] to physically move a legacy file into `.cimp/`;
/// this resolver is the read-only fallback that keeps a customization readable
/// even when that move can't happen (read-only child, or a failed rename).
fn overlay_read_path(launch_cwd: &Path) -> PathBuf {
    let canonical = custom_path(launch_cwd);
    if canonical.exists() {
        return canonical;
    }
    let legacy = legacy_custom_path(launch_cwd);
    if legacy.exists() {
        return legacy;
    }
    canonical
}

/// Best-effort move of a pre-consolidation loose overlay into `.cimp/`. No-op
/// when the canonical file already exists or no legacy file is present. A
/// failed move leaves the legacy file untouched — [`overlay_read_path`] still
/// finds it, so no customization is lost and the migration simply retries on
/// the next launch. Only the side-effectful [`load`] calls this; the read-only
/// child path never moves the user's files.
fn migrate_legacy_overlay(launch_cwd: &Path) {
    let canonical = custom_path(launch_cwd);
    let legacy = legacy_custom_path(launch_cwd);
    if canonical.exists() || !legacy.exists() {
        return;
    }
    if let Some(parent) = canonical.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            tracing::warn!(
                error = %e,
                dir = %parent.display(),
                "settings: create .cimp dir for overlay migration failed; reading legacy overlay in place"
            );
            return;
        }
    }
    match fs::rename(&legacy, &canonical) {
        Ok(()) => tracing::info!(
            from = %legacy.display(),
            to = %canonical.display(),
            "settings: migrated per-folder overlay into .cimp/"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            from = %legacy.display(),
            to = %canonical.display(),
            "settings: overlay migration move failed; reading legacy in place, will retry next launch"
        ),
    }
}

/// Bundle returned from `load`: the resolved settings plus a snapshot of
/// just the global baseline. The saver needs the baseline so it can diff
/// the live state against it on every write without re-reading the file.
pub struct LoadOutcome {
    pub settings: Settings,
    pub global: Settings,
}

/// Always returns a `LoadOutcome`. Defaults are written to disk for the
/// global baseline when it's absent or corrupt; the custom overlay is
/// merely skipped if absent and quarantined if corrupt.
///
/// Migration runs on the global value only. The overlay is a partial diff
/// always written in the current schema, so it is merged as-is (see the
/// inline note at step 2 for why running the legacy cascade on a partial
/// overlay caused silent data loss and unbounded `.bak` growth).
pub fn load(default_shell: &ShellSpec, launch_cwd: &Path) -> LoadOutcome {
    // 1. Load and migrate the global baseline. After this `global` is in
    //    the current schema shape; a v1.x file on disk has been backed up
    //    next to the global path and rewritten.
    let mut global = load_global(default_shell);

    // 2. Load the overlay (if any). We deliberately DON'T run the legacy
    //    migration cascade on it: the overlay is a *partial* diff (see
    //    `diff`), always written by this app's `save` in the current schema.
    //    The migration detectors (`looks_v1_2` etc.) key off absent top-level
    //    fields, which a partial overlay legitimately lacks — so migrating it
    //    both stamps full-object defaults that override global through the
    //    merge (silent data loss) and, because the overlay never gains a
    //    `schema_version`, re-fires every launch (unbounded `.v1.2.bak`
    //    growth). Missing fields are handled correctly downstream anyway:
    //    deep-merge fills from the (already-migrated) global, and serde
    //    `#[serde(default)]` covers the rest.
    //    First fold any pre-consolidation loose overlay into `.cimp/`, then
    //    read from the resolved location (canonical `.cimp/config.json`, or the
    //    legacy file if the move couldn't happen).
    migrate_legacy_overlay(launch_cwd);
    let overlay_value = read_overlay(&overlay_read_path(launch_cwd), true).map(|mut v| {
        // Per-install fields never belong in an overlay (see
        // `OVERLAY_BANNED_KEYS`) — drop them before the merge so an overlay
        // contaminated by a pre-guard version can't shadow the global file.
        strip_overlay_banned(&mut v);
        v
    });

    // 3. Merge the (now both-current-shape) global + overlay.
    let mut merged = match serde_json::to_value(&global) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "settings: serialize global to value failed; using global as-is");
            return LoadOutcome {
                settings: global.clone(),
                global,
            };
        }
    };
    let overlay_existed = overlay_value.is_some();
    if let Some(overlay) = overlay_value {
        deep_merge(&mut merged, overlay);
    }

    let mut settings: Settings = match serde_json::from_value(merged) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "settings: typed parse failed (post-merge); using global"
            );
            return LoadOutcome {
                settings: global.clone(),
                global,
            };
        }
    };

    let repaired = integrity_check(&mut settings);

    // Re-point bundled avatar videos at the loaded theme. Existing installs
    // had absolute paths frozen to the seed-time theme written into their
    // settings file; this corrects them in memory on every launch so the
    // avatar matches `ui.theme`. Done after the merge (so the resolved
    // theme is known) and kept out of the persisted diff — it re-derives
    // from theme + on-disk folder each load, so there's nothing to save.
    apply_portable_avatar_paths(&mut settings);
    // Stamp the baseline identically. The avatar paths are re-derived from
    // `ui.theme` + the on-disk folder each load, so they must be present on
    // BOTH sides of every diff to cancel out. Without this, after the user
    // changes `ui.theme` the stamped (new-theme) paths in `settings` differ
    // from the on-disk `global` (frozen at the seed-time theme) and the diff
    // writes machine-specific absolute avatar paths into the portable
    // per-folder overlay, pinning the avatar to the save-time theme.
    apply_portable_avatar_paths(&mut global);
    // Apply the SAME idempotent integrity repairs to the baseline we return.
    // This `global` is what the long-lived saver task (broadcaster) and every
    // later `set()` diff the live (integrity-checked) settings against. Without
    // it, repairs enforced only on `settings` (restored AI builtins, the
    // materialized offload-server tab, canonical flags) read as user
    // customizations on every save and leak into the portable per-folder
    // overlay. The load-time post-repair save below relies on this too.
    let _ = integrity_check(&mut global);

    if repaired {
        // Persist the post-repair state back to its source of truth. If a
        // custom overlay was in play, we recompute and rewrite the diff;
        // otherwise we rewrite global.
        if overlay_existed {
            // `global` is already integrity-checked (above), so invariants
            // enforced on both sides don't get mistaken for user
            // customizations and written into the overlay.
            if let Err(e) = save(&settings, launch_cwd, &global) {
                tracing::warn!(error = %e, "settings: post-repair save (custom) failed");
            }
        } else if let Err(e) = save_global(&settings) {
            tracing::warn!(error = %e, "settings: post-repair save (global) failed");
        }
    }

    LoadOutcome { settings, global }
}

/// V8-01: read-only settings load for a lightweight subprocess (the
/// `cimp --offload-mcp` child). Reads the global baseline + per-folder
/// overlay and deserializes, with **no** side effects — no writes, no
/// migration, no quarantine, no integrity repair. The offload block is
/// additive (`#[serde(default)]`), so a current-schema file parses with
/// `offload` defaulted when absent; anything unreadable falls back to
/// `Settings::default()` (offload disabled). The child only consumes a
/// handful of `offload` fields, so the lighter path avoids dragging the
/// shell-probe + disk-write machinery into a per-Claude-session spawn.
pub fn load_readonly(launch_cwd: &Path) -> Settings {
    let global = global_path()
        .ok()
        .filter(|p| p.exists())
        .and_then(|p| fs::read_to_string(&p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let mut merged = match global {
        Some(v) => v,
        None => return Settings::default(),
    };
    if let Some(mut overlay) = read_overlay(&overlay_read_path(launch_cwd), false) {
        strip_overlay_banned(&mut overlay);
        deep_merge(&mut merged, overlay);
    }
    serde_json::from_value(merged).unwrap_or_default()
}

/// V14 Phase A: global scope of the prompt-template library, read directly
/// from the physical global file (`<exe-dir>/settings.json`) — NOT from the
/// live merged `Settings`, which (if the active project's overlay happens to
/// carry its own `prompt_templates` key) would already show the OVERLAY's
/// array in that field per the deep-merge's array-replace-wholesale rule.
/// Reading the true global file directly is what makes "global" actually
/// mean "every project", independent of which one cImp is launched from.
/// Missing/corrupt file → empty (never errors; a fresh install with no
/// global file yet has no templates to show).
pub fn read_global_prompt_templates() -> Vec<PromptTemplate> {
    match global_path() {
        Ok(p) => read_prompt_templates_from(&p),
        Err(_) => Vec::new(),
    }
}

fn read_prompt_templates_from(path: &Path) -> Vec<PromptTemplate> {
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .map(|s| s.prompt_templates)
        .unwrap_or_default()
}

/// V14 Phase A: write the prompt-template library straight to the physical
/// global file, bypassing the normal per-project overlay diff every other
/// Settings-window edit goes through (`settings_update` → `SettingsHandle`'s
/// debounced saver, which diffs against the pristine global snapshot and
/// writes into whichever project's `.cimp/config.json` is active). Without
/// this bypass, editing the "global" library from a Settings window opened
/// inside a customized project would silently land in that project's
/// overlay instead — defeating the whole global/project split this feature
/// promises. Read-modify-write: every other field in the on-disk global
/// file is preserved untouched.
pub fn write_global_prompt_templates(templates: Vec<PromptTemplate>) -> AppResult<()> {
    let path = global_path()?;
    write_prompt_templates_to(&path, templates)
}

/// Read-modify-write base shared by every out-of-band global writer: the
/// physical file parsed as `Settings`, or defaults when missing/corrupt (a
/// corrupt file is the normal `load` path's problem — these writers must
/// still be able to record their one field).
fn read_settings_or_default(path: &Path) -> Settings {
    if !path.exists() {
        return Settings::default();
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_prompt_templates_to(path: &Path, templates: Vec<PromptTemplate>) -> AppResult<()> {
    let mut settings = read_settings_or_default(path);
    settings.prompt_templates = templates;
    // An explicit Settings-window save always counts as "seeded" — it must
    // never be clobbered by the one-time starter injection on a later load.
    settings.templates_seeded = true;
    save_to(path, &settings)
}

/// Global-only LLM price table, read directly from the physical global file
/// for the same reason as [`read_global_prompt_templates`]: the deep-merge
/// would replace the array wholesale if a project overlay happened to carry
/// the key, and "global" must mean every project. Missing file → the seeded
/// defaults (a fresh install shows current Anthropic/Copilot prices); a file
/// that carries the key — even as `[]` — keeps exactly what it has.
pub fn read_global_llm_pricing() -> Vec<LlmPricingModel> {
    let Ok(path) = global_path() else {
        return crate::settings::default_llm_pricing();
    };
    read_llm_pricing_from(&path)
}

fn read_llm_pricing_from(path: &Path) -> Vec<LlmPricingModel> {
    if !path.exists() {
        return crate::settings::default_llm_pricing();
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .map(|s| s.llm_pricing)
        .unwrap_or_else(crate::settings::default_llm_pricing)
}

/// Write the LLM price table straight to the physical global file, bypassing
/// the per-project overlay diff — mirror of [`write_global_prompt_templates`]
/// (see its doc comment for why the bypass exists). Read-modify-write: every
/// other field in the on-disk global file is preserved untouched.
pub fn write_global_llm_pricing(pricing: Vec<LlmPricingModel>) -> AppResult<()> {
    let path = global_path()?;
    write_llm_pricing_to(&path, pricing)
}

fn write_llm_pricing_to(path: &Path, pricing: Vec<LlmPricingModel>) -> AppResult<()> {
    let mut settings = read_settings_or_default(path);
    settings.llm_pricing = pricing;
    save_to(path, &settings)
}

/// Mtime-gated cache for [`read_global_harness_versions`]: the Advisor card
/// polls it every ~2s and tab spawns consult it, but the answer only changes
/// when the global file does — re-read + re-parse the whole `Settings` only
/// when the file's mtime moved (a hand edit or an out-of-process write still
/// shows up through the mtime), otherwise a metadata stat is the entire
/// cost. In-process writers ([`mutate_global_harness_versions`]) refresh the
/// cache directly so a same-timestamp write can't serve a stale value.
static HV_CACHE: std::sync::Mutex<Option<(std::time::SystemTime, HarnessVersions)>> =
    std::sync::Mutex::new(None);

/// V16 Feature 1: harness version + contract state, read straight from the
/// physical global file for the same reason as [`read_global_llm_pricing`] —
/// it's per-install state and must never be shadowed by (or diffed into) a
/// project overlay. Missing/corrupt file → defaults (all-unverified).
/// Cheap to call from polls and spawn paths (see [`HV_CACHE`]).
pub fn read_global_harness_versions() -> HarnessVersions {
    let Ok(path) = global_path() else {
        return HarnessVersions::default();
    };
    let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else {
        return HarnessVersions::default();
    };
    if let Ok(cache) = HV_CACHE.lock() {
        if let Some((cached_at, hv)) = cache.as_ref() {
            if *cached_at == mtime {
                return hv.clone();
            }
        }
    }
    let hv = read_settings_or_default(&path).harness_versions;
    if let Ok(mut cache) = HV_CACHE.lock() {
        *cache = Some((mtime, hv.clone()));
    }
    hv
}

/// Mutate the global `harness_versions` state in place (read-modify-write on
/// the physical global file, every other field preserved — mirror of
/// [`write_global_prompt_templates`]). Returns the post-mutation state.
/// No-ops (no disk write) when the mutation leaves the state unchanged, so
/// background callers polling a version can call this freely.
pub fn mutate_global_harness_versions(
    mutate: impl FnOnce(&mut HarnessVersions),
) -> AppResult<HarnessVersions> {
    let path = global_path()?;
    let mut settings = read_settings_or_default(&path);
    let before = settings.harness_versions.clone();
    mutate(&mut settings.harness_versions);
    if settings.harness_versions == before {
        return Ok(before);
    }
    let after = settings.harness_versions.clone();
    save_to(&path, &settings)?;
    // Refresh the read cache under the post-write mtime — never leave it
    // holding the pre-write value for a same-timestamp write.
    if let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) {
        if let Ok(mut cache) = HV_CACHE.lock() {
            *cache = Some((mtime, after.clone()));
        }
    }
    Ok(after)
}

/// Record a harness version observation (V16 Feature 1's tripwire input).
/// `harness` is `"claude"` (from the OOB transcript tap) or `"opencode"`
/// (from `opencode --version` at tab spawn). Change-guarded — safe to call
/// once per session/spawn without file churn.
pub fn note_harness_version(harness: &str, version: &str) {
    let version = version.trim();
    if version.is_empty() {
        return;
    }
    let res = mutate_global_harness_versions(|hv| match harness {
        "claude" => hv.claude_last_seen = version.to_string(),
        "opencode" => hv.opencode_last_seen = version.to_string(),
        _ => {}
    });
    if let Err(e) = res {
        tracing::warn!("failed to record {harness} version {version}: {e}");
    }
}

/// V14 Phase A: project scope of the prompt-template library, read directly
/// from the raw overlay JSON at `root` (its own `prompt_templates` key) —
/// bypassing the typed, deep-merged `Settings` for exactly the same reason
/// [`read_global_prompt_templates`] bypasses it. Missing overlay, missing
/// key, or a malformed array all degrade to empty rather than erroring —
/// project templates are a nice-to-have, not load-bearing for the app to
/// start.
pub fn read_project_prompt_templates(root: &Path) -> Vec<PromptTemplate> {
    let path = overlay_read_path(root);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("prompt_templates")
        .and_then(|v| serde_json::from_value::<Vec<PromptTemplate>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Read the global file. Writes seeded defaults when absent. On parse
/// failure quarantines the file and returns defaults. Runs migration on
/// the global file in place — backup goes next to the global path itself,
/// not next to whatever path the merged result resolved to.
fn load_global(default_shell: &ShellSpec) -> Settings {
    let path = match global_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "settings: cannot resolve global path; using defaults");
            return seeded_defaults(default_shell);
        }
    };

    if !path.exists() {
        let s = seeded_defaults(default_shell);
        if let Err(e) = save_to(&path, &s) {
            tracing::warn!(error = %e, path = %path.display(), "settings: write global defaults failed");
        } else {
            tracing::info!(path = %path.display(), "settings: wrote global defaults");
        }
        return s;
    }

    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read global failed; using defaults");
            return seeded_defaults(default_shell);
        }
    };

    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: parse global failed; quarantining and resetting"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            return s;
        }
    };

    // Migrate the global file in place. Backup is named after the global
    // file, which is the source of truth for the global baseline shape.
    let migrated = match migration::migrate_if_needed(&mut value, &path, default_shell) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                path = %path.display(),
                "settings: global migration aborted (backup failed); using defaults"
            );
            return seeded_defaults(default_shell);
        }
    };

    let mut typed: Settings = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: typed parse of global failed; quarantining and resetting"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            return s;
        }
    };

    // V14 Phase A: seed the starter prompt-template library exactly once,
    // directly against the physical global file — this is the ONE place
    // that runs (unlike `integrity_check`, which also runs against the
    // per-project merged `Settings` and wouldn't reliably flush a fresh
    // seed to disk when a project overlay is active; see the function's
    // own doc comment).
    let seeded = seed_prompt_templates_if_needed(&mut typed);

    if migrated || seeded {
        // Persist the migrated/seeded shape back to disk so future launches
        // don't re-migrate or re-seed. Atomic write inside save_to keeps
        // this safe under crash.
        if let Err(e) = save_to(&path, &typed) {
            tracing::warn!(error = %e, path = %path.display(), "settings: post-migration/seed global save failed");
        } else {
            tracing::info!(path = %path.display(), migrated, seeded, "settings: global migrated/seeded and rewritten");
        }
    } else {
        tracing::info!(path = %path.display(), "settings: global loaded");
    }
    typed
}

/// Read and parse the custom overlay file as a generic `Value`. Returns
/// `None` if absent. On parse failure the file is quarantined and `None`
/// is returned — we want the app to come up cleanly even if a hand-edit
/// broke the overlay.
fn read_overlay(path: &Path, quarantine: bool) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read overlay failed; ignoring");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            // `quarantine` is false for the read-only child path
            // (`load_readonly`), whose contract is no side effects — a child
            // process must never move the user's overlay file.
            if quarantine {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "settings: parse overlay failed; quarantining"
                );
                migration::quarantine_corrupt_file(path);
            } else {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "settings: parse overlay failed; ignoring (read-only load)"
                );
            }
            None
        }
    }
}

/// Top-level `Settings` fields that are PER-INSTALL state, written straight
/// to the physical global file by dedicated writers
/// (`write_global_llm_pricing`, `mutate_global_harness_versions`) — and must
/// therefore never appear in a per-project overlay: not written into one by
/// [`save`]'s diff (those writers bypass the in-memory `global` baseline, so
/// a later unrelated save would see the field as "changed" and pin the stale
/// value into the overlay, shadowing every future global write for that
/// project), and not honored from one at load ([`load`] strips them before
/// the merge, which also heals overlays contaminated before this guard
/// existed).
///
/// NOT the same set as the fields `apply_incoming_settings`
/// (ipc/commands.rs) preserves across a Settings-window round trip:
/// `prompt_templates`/`templates_seeded` are out-of-band there too, but a
/// project overlay legitimately carries its own project-scoped template
/// library ([`read_project_prompt_templates`]), so they are NOT banned here.
const OVERLAY_BANNED_KEYS: &[&str] = &["llm_pricing", "harness_versions"];

fn strip_overlay_banned(v: &mut Value) {
    if let Value::Object(map) = v {
        for k in OVERLAY_BANNED_KEYS {
            map.remove(*k);
        }
    }
}

/// Write the diff between `settings` and `global` to the custom overlay
/// file in `launch_cwd`. If the diff is empty, deletes any existing
/// overlay (so a user who reverts every change ends up with a clean
/// directory).
pub fn save(settings: &Settings, launch_cwd: &Path, global: &Settings) -> AppResult<()> {
    let path = custom_path(launch_cwd);
    let mut current = serde_json::to_value(settings)
        .map_err(|e| AppError::Settings(format!("serialize current: {e}")))?;
    let mut baseline = serde_json::to_value(global)
        .map_err(|e| AppError::Settings(format!("serialize global: {e}")))?;
    strip_overlay_banned(&mut current);
    strip_overlay_banned(&mut baseline);

    match diff(&current, &baseline) {
        Some(delta) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(AppError::Io)?;
            }
            let text = serde_json::to_string_pretty(&delta)
                .map_err(|e| AppError::Settings(format!("serialize overlay: {e}")))?;
            write_atomic(&path, text.as_bytes())?;
        }
        None => {
            if path.exists() {
                // Propagate, don't swallow: if the now-empty overlay can't be
                // removed it stays on disk and is re-merged next launch,
                // silently undoing the user's revert. The caller must learn the
                // save did not actually take effect.
                fs::remove_file(&path).map_err(|e| {
                    tracing::warn!(error = %e, path = %path.display(), "settings: remove empty overlay failed");
                    AppError::Io(e)
                })?;
            }
        }
    }
    Ok(())
}

/// Write the full settings to the global file. Only used during initial
/// seeding and to finalize a post-migration / post-integrity rewrite when
/// no custom overlay is in play.
pub fn save_global(settings: &Settings) -> AppResult<()> {
    let path = global_path()?;
    save_to(&path, settings)
}

fn save_to(path: &Path, settings: &Settings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Settings(format!("serialize: {e}")))?;
    write_atomic(path, text.as_bytes())
}

/// Recursively merge `overlay` into `base`. Objects are merged key-by-key;
/// every other value (arrays, primitives, null) replaces wholesale.
fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (k, v) in overlay_map {
                match base_map.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (slot, overlay) => {
            *slot = overlay;
        }
    }
}

/// Compute the minimal `current - baseline` overlay. Returns `None` when
/// the two are equal (i.e. nothing to write). Objects are diffed
/// key-by-key — keys whose values match the baseline are omitted, keys
/// that differ are included with the current value, keys present in the
/// baseline but missing from current are emitted as JSON null so the
/// reverse merge can reconstruct the deletion. Arrays and primitives are
/// included whole if they differ at all.
fn diff(current: &Value, baseline: &Value) -> Option<Value> {
    if current == baseline {
        return None;
    }
    match (current, baseline) {
        (Value::Object(c), Value::Object(b)) => {
            let mut out = Map::new();
            for (k, cv) in c {
                match b.get(k) {
                    Some(bv) => {
                        if let Some(sub) = diff(cv, bv) {
                            out.insert(k.clone(), sub);
                        }
                    }
                    None => {
                        out.insert(k.clone(), cv.clone());
                    }
                }
            }
            for (k, _) in b {
                if !c.contains_key(k) {
                    out.insert(k.clone(), Value::Null);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        _ => Some(current.clone()),
    }
}

/// `Settings::default()` with the three default tab entries seeded for the
/// host platform and the portable on-disk avatar paths stamped in (when
/// present). Used on fresh installs and as the recovery fallback when the
/// file is unrecoverable.
///
/// The integrity check restores the AI builtins; we additionally seed
/// `shell-default-1` here so a brand-new install gets one ready-to-use
/// shell tab. The integrity check no longer re-creates that tab on its
/// own, so closing it once is permanent (which is what we want for a
/// closable tab).
fn seeded_defaults(default_shell: &ShellSpec) -> Settings {
    let mut s = Settings::default();
    integrity_check(&mut s);
    s.tabs.push(default_shell_1_tab(default_shell));
    // V16: broot is no longer auto-seeded. It (and rustnet) launch on demand
    // from the bottom-bar tool buttons into ordinary closable Shell tabs.
    apply_portable_avatar_paths(&mut s);
    // V14 Phase A: fresh installs get the 4 starter prompt templates.
    seed_prompt_templates_if_needed(&mut s);
    s
}

/// V14 Phase A: seed [`starter_prompt_templates`] into `prompt_templates`
/// exactly once, gated on `templates_seeded` rather than the schema version
/// (see that field's doc comment). Returns `true` when it actually seeded
/// (i.e. this was the first load), so callers know whether the physical
/// global file needs rewriting. Idempotent — a second call is a no-op.
fn seed_prompt_templates_if_needed(s: &mut Settings) -> bool {
    if s.templates_seeded {
        return false;
    }
    s.prompt_templates = starter_prompt_templates();
    s.templates_seeded = true;
    true
}

/// `<exe-dir>/../avatars/` — the portable avatar folder shipped in the
/// release zip. `None` if the exe path can't be resolved (which would
/// only happen on platforms or sandboxes where `current_exe()` fails) or
/// the folder doesn't actually exist (dev `cargo run`, or someone built
/// from source without staging).
fn portable_avatars_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("avatars");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Point the bundled avatar defaults at the active theme's videos under
/// `<exe-dir>/../avatars/<theme>/` so the on-disk copies users can swap are
/// the live source. Re-pointing follows the active theme, so callers run it
/// on first-launch seeding, on load (to repair a settings file seeded under
/// a different theme), and on every theme change. Genuine user overrides
/// (paths outside the portable avatars dir) are preserved; see
/// [`stamp_avatar_paths_from`]. No-op when the portable folder is absent
/// (dev `cargo run` / source builds), leaving the embedded `/avatar/...`
/// defaults in place.
pub fn apply_portable_avatar_paths(s: &mut Settings) {
    if let Some(dir) = portable_avatars_dir() {
        stamp_avatar_paths_from(s, &dir);
    }
}

/// Pure version of the avatar stamping step: given a directory, re-point the
/// bundled avatar defaults at the active theme's videos inside it. Split out
/// so the behavior is unit-testable without depending on the test binary's
/// `current_exe()` location.
///
/// Layout-aware: prefers the active theme's subfolder
/// (`<dir>/<ui.theme>/<file>`) so the avatar follows the chrome theme.
/// Falls back to a flat `<dir>/<file>` layout for legacy zips produced
/// before the per-theme split, so existing folders keep working. Per the
/// theme isolation policy (see `src/theme.css`), every theme owns its own
/// avatar folder — there is no cross-theme mapping here.
///
/// Idempotent and safe to call on every load and theme change: it only
/// re-points fields that are *bundled defaults* — unset, a bundled
/// `/avatar/...` URL, or a path we previously stamped inside `dir` (for any
/// theme). An absolute path *outside* `dir` is a deliberate user override
/// (picked from elsewhere on disk via the file dialog) and is left alone.
/// This is what makes the avatar videos follow theme switches instead of
/// staying frozen on whichever theme was active when the global settings
/// file was first seeded.
fn stamp_avatar_paths_from(s: &mut Settings, dir: &Path) {
    let theme_dir = dir.join(&s.ui.theme);

    let pick = |file: &str| -> Option<PathBuf> {
        let themed = theme_dir.join(file);
        if themed.is_file() {
            return Some(themed);
        }
        let flat = dir.join(file);
        if flat.is_file() {
            return Some(flat);
        }
        None
    };

    // Bundled default = re-pointable. A None field, a bundled `/avatar/...`
    // URL, or any path under `dir` (something we stamped before, possibly
    // for a different theme) all qualify. A path outside `dir` is a real
    // user override and is never disturbed.
    let is_bundled = |p: &Option<PathBuf>| match p {
        None => true,
        Some(path) => {
            path.starts_with(dir)
                || path.to_str().is_some_and(|s| s.starts_with("/avatar/"))
        }
    };

    if is_bundled(&s.avatar.images.idle) {
        s.avatar.images.idle = pick("Idle.mp4");
    }
    if is_bundled(&s.avatar.images.listening) {
        s.avatar.images.listening = pick("Listening.mp4");
    }
    if is_bundled(&s.avatar.images.thinking) {
        s.avatar.images.thinking = pick("Thinking.mp4");
    }
    if is_bundled(&s.avatar.images.speaking) {
        s.avatar.images.speaking = pick("Speaking.mp4");
    }
    if is_bundled(&s.avatar.images.error) {
        s.avatar.images.error = pick("Error.mp4");
    }
    // Transition keeps a non-None bundled fallback (`/avatar/Transition.mp4`,
    // which the frontend redirects into the active theme) when the theme
    // ships no on-disk file, so a theme switch never silently disables the
    // crossfade by leaving it None.
    if is_bundled(&s.avatar.transition.path) {
        s.avatar.transition.path =
            pick("Transition.mp4").or_else(|| Some(PathBuf::from("/avatar/Transition.mp4")));
    }
}

/// Drop any reserved AI tab that is not in `enabled_ai_tabs`. Returns
/// `true` if any entry was removed.
fn drop_disabled_ai_builtins(settings: &mut Settings) -> bool {
    let want: std::collections::HashSet<&'static str> = settings
        .enabled_ai_tabs
        .iter()
        .map(|id| id.as_str())
        .collect();
    let before = settings.tabs.len();
    settings.tabs.retain(|t| {
        // Keep anything that isn't a reserved AI id, plus reserved ids
        // that are in the want-set.
        let id = t.id();
        if AiTabId::from_id(id).is_some() {
            want.contains(id)
        } else {
            true
        }
    });
    let removed = before != settings.tabs.len();
    if removed {
        tracing::warn!("integrity: dropped reserved AI tab(s) not in enabled_ai_tabs");
    }
    removed
}

/// For each id in `enabled_ai_tabs` that's missing from `tabs`, insert
/// the default config at the canonical position. Returns `true` if any
/// entry was inserted.
fn restore_enabled_ai_builtins(settings: &mut Settings) -> bool {
    // Iterate in canonical order (claude → claude-local → opencode) so
    // successive insertions land in the right relative
    // slot regardless of the user's `enabled_ai_tabs` ordering.
    let order = [
        AiTabId::Claude,
        AiTabId::ClaudeLocal,
        AiTabId::OpenCode,
    ];
    let mut changed = false;
    for &id in &order {
        if !settings.enabled_ai_tabs.contains(&id) {
            continue;
        }
        if settings.tabs.iter().any(|t| t.id() == id.as_str()) {
            continue;
        }
        let pos = canonical_insert_position(&settings.tabs, id);
        settings.tabs.insert(pos, default_ai_tab(id));
        changed = true;
        tracing::warn!(id = id.as_str(), "integrity: restored missing AI builtin tab");
    }
    changed
}

/// Position to insert a freshly-restored reserved AI tab so the AI
/// builtins keep their canonical leading order. Walks the existing
/// `tabs[]` and lands the new entry after the last reserved AI tab
/// whose canonical order is < `id`'s — so reinserting `aider` after the
/// user has `claude`, `claude-local`, and a shell tab places aider at
/// index 2 (in front of the shell), not at the end.
fn canonical_insert_position(tabs: &[TabConfig], id: AiTabId) -> usize {
    let target = id.canonical_order();
    let mut pos = 0usize;
    for (idx, tab) in tabs.iter().enumerate() {
        match AiTabId::from_id(tab.id()) {
            Some(other) if other.canonical_order() < target => {
                pos = idx + 1;
            }
            // First non-reserved-AI tab (or a higher-canonical-order AI
            // tab) marks the upper bound — anything past here is a
            // shell or a later-AI tab and we should insert before it.
            _ => return pos,
        }
    }
    pos
}

/// One row per reserved feature tab, in canonical left-to-right tab-strip
/// order. THE single persistence-side registry of the reserved dashboards:
/// the reconcile loop derives presence (from `enabled`), the default entry,
/// the integrity log lines, and the insert position (contiguous after the
/// leading AI builtins, in this array's order) from the row — adding the
/// next reserved tab is one new row here, not a new copy-pasted
/// `reconcile_*_tab` + `*_insert_position` pair.
struct ReservedTabSpec {
    /// The reserved tab id (a `*_TAB_ID` constant).
    id: &'static str,
    /// Display label for the integrity log lines.
    log_name: &'static str,
    /// The gating flag's name, as it reads in the integrity log lines.
    flag: &'static str,
    /// Reads the settings flag that gates the tab's presence.
    enabled: fn(&Settings) -> bool,
    /// Builds the default `TabConfig` materialized when the flag turns on.
    default_tab: fn() -> TabConfig,
    /// Re-force the display name to the default on every reconcile — how a
    /// rename reaches existing installs (the tab is persisted and never
    /// re-materialized; the V10 Code Graph → Code Intelligence rename shipped
    /// this way). The older Offload Server / Workbench tabs predate the
    /// mechanism and keep whatever name the file carries.
    sync_name: bool,
}

const RESERVED_TAB_SPECS: &[ReservedTabSpec] = &[
    ReservedTabSpec {
        id: OFFLOAD_SERVER_TAB_ID,
        log_name: "Offload Server",
        flag: "offload",
        enabled: |s| s.offload.enabled,
        default_tab: default_offload_server_tab,
        sync_name: false,
    },
    ReservedTabSpec {
        id: GRAPH_MONITOR_TAB_ID,
        log_name: "Code Graph monitor",
        flag: "graph",
        enabled: |s| s.graph.enabled,
        default_tab: default_graph_monitor_tab,
        sync_name: true,
    },
    ReservedTabSpec {
        id: WORKBENCH_TAB_ID,
        log_name: "Workbench",
        flag: "workbench",
        enabled: |s| s.workbench.enabled,
        default_tab: default_workbench_tab,
        sync_name: false,
    },
    ReservedTabSpec {
        id: GRAPH_VIEW_TAB_ID,
        log_name: "Graph View",
        flag: "graph_viz",
        enabled: |s| s.graph.graph_viz,
        default_tab: default_graph_view_tab,
        sync_name: true,
    },
    ReservedTabSpec {
        id: TOOL_ACTIVITY_TAB_ID,
        log_name: "Tool Activity",
        flag: "tool_activity_tab",
        enabled: |s| s.ui.tool_activity_tab,
        default_tab: default_tool_activity_tab,
        sync_name: true,
    },
    ReservedTabSpec {
        id: CODE_AUDIT_TAB_ID,
        log_name: "Code Audit",
        flag: "code_audit",
        enabled: |s| s.code_audit.enabled,
        default_tab: default_code_audit_tab,
        sync_name: true,
    },
    // V25: the Code Quality tab shares the `code_audit.enabled` flag with Code
    // Audit — enabling the feature materializes both tabs, contiguous and in
    // this order (Code Audit then Code Quality).
    ReservedTabSpec {
        id: CODE_QUALITY_TAB_ID,
        log_name: "Code Quality",
        flag: "code_audit",
        enabled: |s| s.code_audit.enabled,
        default_tab: default_code_quality_tab,
        sync_name: true,
    },
];

/// Keep one reserved feature tab present iff its gating flag is on. Inserts
/// it at its canonical position when enabling, removes it when disabling,
/// and re-forces `builtin: true` (plus the default display name when
/// `sync_name`) on a surviving entry so a hand-edit can't make it closable.
/// Returns `true` if the tabs array changed.
fn reconcile_reserved_tab(settings: &mut Settings, spec: &ReservedTabSpec) -> bool {
    let present = settings.tabs.iter().position(|t| t.id() == spec.id);
    if (spec.enabled)(settings) {
        match present {
            Some(i) => {
                let mut changed = false;
                if !settings.tabs[i].builtin() {
                    settings.tabs[i].set_builtin(true);
                    changed = true;
                }
                if spec.sync_name {
                    let want_name = (spec.default_tab)().name().to_string();
                    if settings.tabs[i].name() != want_name {
                        settings.tabs[i].set_name(want_name);
                        changed = true;
                    }
                }
                changed
            }
            None => {
                let pos = reserved_tab_insert_position(&settings.tabs, spec.id);
                settings.tabs.insert(pos, (spec.default_tab)());
                tracing::info!(
                    "integrity: materialized {} tab ({} enabled)",
                    spec.log_name,
                    spec.flag
                );
                true
            }
        }
    } else if let Some(i) = present {
        settings.tabs.remove(i);
        tracing::info!(
            "integrity: removed {} tab ({} disabled)",
            spec.log_name,
            spec.flag
        );
        true
    } else {
        false
    }
}

/// Insert position for the reserved tab `id`: after the contiguous leading
/// AI builtins AND every reserved tab that precedes `id` in
/// [`RESERVED_TAB_SPECS`] order, ahead of user shells — the "reserved
/// feature tabs stay contiguous, leftmost, in canonical order" rule.
fn reserved_tab_insert_position(tabs: &[TabConfig], id: &str) -> usize {
    let rank = RESERVED_TAB_SPECS
        .iter()
        .position(|s| s.id == id)
        .unwrap_or(RESERVED_TAB_SPECS.len());
    let mut pos = 0usize;
    for (idx, tab) in tabs.iter().enumerate() {
        let earlier_reserved = RESERVED_TAB_SPECS[..rank].iter().any(|s| s.id == tab.id());
        if AiTabId::from_id(tab.id()).is_some() || earlier_reserved {
            pos = idx + 1;
        } else {
            break;
        }
    }
    pos
}

/// Re-run every reserved feature-tab reconcile (in [`RESERVED_TAB_SPECS`]
/// order) so the persisted tab list matches the current enable flags. The
/// full [`integrity_check`] only runs at load-from-disk; the live
/// settings-update path calls this so toggling a feature materializes/removes
/// its reserved tab immediately. Returns `true` if `settings.tabs` changed.
pub fn reconcile_reserved_tabs(settings: &mut Settings) -> bool {
    let mut changed = false;
    for spec in RESERVED_TAB_SPECS {
        changed |= reconcile_reserved_tab(settings, spec);
    }
    changed
}

/// V25: reconcile `code_audit.tools` with the built-in adapter set. A config
/// persisted by v0.43/v0.44 (before the Quality tools existed) carries only the
/// three Security entries; the lenient `tools` deserializer keeps a present
/// array verbatim, so those installs would never gain the eleven Quality tools
/// and the Code Quality tab / Settings section would stay empty. This appends a
/// default entry (per [`default_audit_tools`]: enabled except `dotnet-analyzers`
/// and `semgrep-quality`) for every [`AuditToolId`] missing from the array,
/// **preserving every existing entry verbatim and in its current order** —
/// the user's `enabled`/`path`/`extra_args`/`timeout_secs` and any customization
/// survive untouched. Stale/unknown ids were already dropped by the lenient
/// deserializer, and that stays: this only ever *adds* the missing built-ins.
/// Idempotent (a second call finds every id present and is a no-op). Runs on
/// both the load path ([`integrity_check`]) and the live settings-update
/// round-trip (`apply_incoming_settings`), exactly like
/// [`reconcile_reserved_tabs`]. Returns `true` if anything was appended.
pub fn reconcile_audit_tools(settings: &mut Settings) -> bool {
    let tools = &mut settings.code_audit.tools;
    let mut changed = false;
    for def in default_audit_tools() {
        if !tools.iter().any(|t| t.id == def.id) {
            tools.push(def);
            changed = true;
        }
    }
    if changed {
        tracing::info!("integrity: appended missing built-in code_audit tools");
    }
    changed
}

/// All three reserved AI tab ids. Used by the integrity check's "is this
/// id one of our reserved AI builtins?" loops; a single source of truth
/// keeps the `ai_builtins` membership check, the `use_local_provider`
/// expectation table, and the drop-disabled-tab pass in sync.
const AI_BUILTIN_IDS: [&str; 3] = [
    CLAUDE_TAB_ID,
    CLAUDE_LOCAL_TAB_ID,
    OPENCODE_TAB_ID,
];

/// Reconcile the `tabs` array with `enabled_ai_tabs`. Every enabled AI
/// id is forced present and marked `builtin: true`; every reserved AI
/// id absent from the list is dropped from `tabs`. Returns true if
/// anything was changed (caller may want to write back to disk). Logged
/// as a warning when an entry has to be restored — the typical cause is
/// a hand-edited file.
///
/// Restored AI tabs land at their canonical position (claude → 0,
/// claude-local → after claude, opencode → after claude-local).
/// User-created Shell tabs retain their
/// relative ordering after the AI builtins. The `shell-default-1`
/// reserved id is *not* re-seeded here: it's a closable shell that
/// ships only on fresh installs (see `seeded_defaults`).
///
/// Empty `enabled_ai_tabs` (a hand-edit, or a malformed migration) is
/// repaired by forcing it back to `[claude]` so the user always boots
/// with at least one AI tab.
pub fn integrity_check(settings: &mut Settings) -> bool {
    let mut changed = false;

    // 0. Empty enabled_ai_tabs is invalid — repair to [claude].
    if settings.enabled_ai_tabs.is_empty() {
        settings.enabled_ai_tabs = vec![AiTabId::Claude];
        changed = true;
        tracing::warn!("integrity: enabled_ai_tabs was empty; reset to [claude]");
    }

    // 1. Force builtin: true on every reserved AI id if it exists with
    //    builtin: false. Defends against hand-edits trying to flip the flag.
    for tab in settings.tabs.iter_mut() {
        if AI_BUILTIN_IDS.contains(&tab.id()) && !tab.builtin() {
            tab.set_builtin(true);
            changed = true;
            tracing::warn!(id = tab.id(), "integrity: forced builtin: true on AI builtin");
        }
    }

    // 2. Force builtin: false on `shell-default-1`: older settings files
    //    persisted it as `builtin: true`, which would block close_tab.
    //    Closability is now uniform across all shell tabs, so demote any
    //    surviving entry on load.
    for tab in settings.tabs.iter_mut() {
        if tab.id() == SHELL_DEFAULT_TAB_ID && tab.builtin() {
            tab.set_builtin(false);
            changed = true;
            tracing::warn!("integrity: demoted shell-default-1 to builtin: false");
        }
    }

    // 3. Force `use_local_provider` to its canonical value on every
    //    reserved AI tab so a hand-edit can't, e.g., flip the
    //    subscription Claude tab into local-LLM mode (which would
    //    silently route the user's primary tab through their local
    //    proxy).
    for tab in settings.tabs.iter_mut() {
        if let TabConfig::AiTool(c) = tab {
            if let Some(reserved) = AiTabId::from_id(c.id.as_str()) {
                let want = reserved.uses_local_provider();
                if c.use_local_provider != want {
                    c.use_local_provider = want;
                    changed = true;
                    tracing::warn!(
                        id = c.id,
                        "integrity: corrected use_local_provider on reserved AI tab"
                    );
                }
            }
        }
    }

    // 3b. Backfill the `question` notification slot on reserved AI builtins.
    //     The slot was added after the per-slot notification migration, which
    //     only promoted keys already on disk — so every tab that upgraded got
    //     a default-disabled, empty `question` and never the configured
    //     "<tool> has a question" default that fresh installs receive. Seed it
    //     ONLY when the slot is in its pure-default state (disabled AND empty
    //     text): that's the upgrader signature. A user who set custom text, or
    //     who deliberately disabled a populated slot, has non-empty text and is
    //     left untouched.
    for tab in settings.tabs.iter_mut() {
        if let TabConfig::AiTool(c) = tab {
            if let Some(reserved) = AiTabId::from_id(c.id.as_str()) {
                let q = &c.notifications.question;
                if !q.enabled && q.text.is_empty() {
                    if let TabConfig::AiTool(d) = default_ai_tab(reserved) {
                        c.notifications.question = d.notifications.question;
                        changed = true;
                        tracing::info!(
                            id = c.id,
                            "integrity: backfilled default question notification on AI builtin"
                        );
                    }
                }
            }
        }
    }

    // 4. Reconcile AI builtin entries with `enabled_ai_tabs`. The list
    //    is the source of truth for which AI tabs exist: every enabled
    //    id is restored at its canonical position; every reserved id
    //    that's not enabled is dropped (the runtime's
    //    set-enabled-ai-tabs IPC kills the PTY in-session, but on cold
    //    load we just normalize the settings file). shell-default-1 is
    //    intentionally untouched here — it's a regular closable shell.
    if drop_disabled_ai_builtins(settings) {
        changed = true;
    }
    if restore_enabled_ai_builtins(settings) {
        changed = true;
    }

    // 4b. Materialize each reserved feature tab while its gating flag is on,
    //     and remove it otherwise (see RESERVED_TAB_SPECS for the set and its
    //     canonical order). Runs before the layout sanity pass so a
    //     freshly-materialized tab is a valid layout id (and the frontend's
    //     orphan placement drops it into a pane); a removed one is pruned
    //     from the layout by step 5.
    if reconcile_reserved_tabs(settings) {
        changed = true;
    }

    // 4c. Reconcile `code_audit.tools` with the built-in adapter set so a
    //     pre-V25 config (three Security tools only) gains the eleven Quality
    //     tools on load. Existing entries are preserved verbatim; only missing
    //     built-ins are appended. Idempotent.
    if reconcile_audit_tools(settings) {
        changed = true;
    }

    // 5. Backend layout sanity. The frontend owns the deep integrity
    //    walk (orphan placement, empty-pane collapse) — it has the tree
    //    helpers. The backend's job here is just to keep the file
    //    deserializable and stop a hand-edit from referring to dead tab
    //    ids: drop tab_ids that don't exist, and clear invalid
    //    `focused_pane_id` so the frontend's leftmost-leaf fallback
    //    kicks in.
    if let Some(layout) = settings.layout.as_mut() {
        let valid_ids: HashSet<&str> =
            settings.tabs.iter().map(|t| t.id()).collect();
        let mut pane_ids: HashSet<String> = HashSet::new();
        if filter_layout_tab_ids(&mut layout.tree, &valid_ids, &mut pane_ids) {
            changed = true;
            tracing::warn!("integrity: dropped unknown tab ids from layout");
        }
        if !pane_ids.contains(&layout.focused_pane_id) {
            // Pick the leftmost-leaf pane id as a deterministic fallback.
            if let Some(replacement) = leftmost_pane_id(&layout.tree) {
                if layout.focused_pane_id != replacement {
                    tracing::warn!(
                        previous = %layout.focused_pane_id,
                        new = %replacement,
                        "integrity: focused_pane_id no longer exists; reset to leftmost leaf"
                    );
                    layout.focused_pane_id = replacement;
                    changed = true;
                }
            }
        }
    }

    changed
}

/// Walk the layout tree, dropping any `tab_ids` entries that aren't in
/// `valid_ids` (and clearing `active_tab_id` if it was dropped or no
/// longer matches a remaining id). Records every encountered pane id in
/// `pane_ids` so the caller can validate `focused_pane_id` afterwards.
/// Returns `true` if anything was changed.
fn filter_layout_tab_ids(
    node: &mut LayoutNodePersisted,
    valid_ids: &HashSet<&str>,
    pane_ids: &mut HashSet<String>,
) -> bool {
    match node {
        LayoutNodePersisted::Pane {
            id,
            tab_ids,
            active_tab_id,
        } => {
            pane_ids.insert(id.clone());
            let before = tab_ids.len();
            tab_ids.retain(|t| valid_ids.contains(t.as_str()));
            let mut changed = tab_ids.len() != before;
            if let Some(active) = active_tab_id.as_deref() {
                if !tab_ids.iter().any(|t| t == active) {
                    *active_tab_id = tab_ids.first().cloned();
                    changed = true;
                }
            }
            changed
        }
        LayoutNodePersisted::Split { first, second, .. } => {
            let mut changed = filter_layout_tab_ids(first, valid_ids, pane_ids);
            changed |= filter_layout_tab_ids(second, valid_ids, pane_ids);
            changed
        }
    }
}

/// Pane id of the leftmost leaf in `node`. Used as the deterministic
/// fallback when `focused_pane_id` no longer maps to an existing pane.
fn leftmost_pane_id(node: &LayoutNodePersisted) -> Option<String> {
    match node {
        LayoutNodePersisted::Pane { id, .. } => Some(id.clone()),
        LayoutNodePersisted::Split { first, .. } => leftmost_pane_id(first),
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

    /// `Settings::default()` with every default-ON reserved feature tab
    /// turned off (currently Workbench + Tool Activity), so tab-count
    /// assertions only see the tabs a test explicitly sets up. A future
    /// default-on reserved tab gets disabled HERE once, not in every test
    /// body. Tests that exercise a specific reserved tab re-enable its flag.
    fn base_test_settings() -> Settings {
        let mut s = Settings::default();
        s.workbench.enabled = false;
        s.ui.tool_activity_tab = false;
        s
    }

    #[test]
    fn integrity_seeds_only_claude_on_empty_with_default_setting() {
        // Default `enabled_ai_tabs = [claude]` means a fresh install
        // gets the subscription Claude tab only; the integrity check
        // mustn't re-seed claude-local. The closable shell-default-1
        // ships via `seeded_defaults`, not the integrity check.
        let mut s = base_test_settings();
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert!(s.tabs[0].builtin());
    }

    #[test]
    fn integrity_seeds_both_when_enabled_ai_tabs_is_both_claudes() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_seeds_only_claude_local_when_setting_is_claude_local_only() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::ClaudeLocal];
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_backfills_default_question_slot_on_upgraded_ai_tab() {
        use crate::settings::schema::{NotificationSlot, TabConfig};
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude];
        integrity_check(&mut s); // seed the claude tab

        // Simulate a file that upgraded before the `question` slot existed:
        // the slot deserialized to the pure default (disabled, empty text).
        if let Some(TabConfig::AiTool(c)) =
            s.tabs.iter_mut().find(|t| t.id() == CLAUDE_TAB_ID)
        {
            c.notifications.question = NotificationSlot::default();
            assert!(!c.notifications.question.enabled);
            assert!(c.notifications.question.text.is_empty());
        }

        let changed = integrity_check(&mut s);
        assert!(changed, "backfill should report a change");
        let q = match s.tabs.iter().find(|t| t.id() == CLAUDE_TAB_ID).unwrap() {
            TabConfig::AiTool(c) => c.notifications.question.clone(),
            _ => panic!("expected AI tab"),
        };
        assert!(q.enabled);
        assert_eq!(q.text, "Claude has a question");
    }

    #[test]
    fn integrity_does_not_clobber_user_customized_question_slot() {
        use crate::settings::schema::{NotificationSlot, TabConfig};
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude];
        integrity_check(&mut s);

        // User deliberately disabled the slot but kept (non-empty) text.
        if let Some(TabConfig::AiTool(c)) =
            s.tabs.iter_mut().find(|t| t.id() == CLAUDE_TAB_ID)
        {
            c.notifications.question = NotificationSlot {
                enabled: false,
                text: "My custom question text".to_string(),
            };
        }

        integrity_check(&mut s);
        let q = match s.tabs.iter().find(|t| t.id() == CLAUDE_TAB_ID).unwrap() {
            TabConfig::AiTool(c) => c.notifications.question.clone(),
            _ => panic!(),
        };
        // Untouched: non-empty text is not the upgrader signature.
        assert!(!q.enabled);
        assert_eq!(q.text, "My custom question text");
    }

    #[test]
    fn integrity_seeds_opencode_at_canonical_position() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![
            AiTabId::Claude,
            AiTabId::ClaudeLocal,
            AiTabId::OpenCode,
        ];
        integrity_check(&mut s);
        assert_eq!(s.tabs.len(), 3);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        assert_eq!(s.tabs[2].id(), OPENCODE_TAB_ID);
    }

    #[test]
    fn integrity_no_offload_tab_when_disabled() {
        let mut s = base_test_settings(); // offload disabled by default
        integrity_check(&mut s);
        assert!(s.tabs.iter().all(|t| t.id() != OFFLOAD_SERVER_TAB_ID));
    }

    #[test]
    fn integrity_materializes_offload_tab_after_ai_builtins() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        s.offload.enabled = true;
        integrity_check(&mut s);
        // Lands right after the two AI builtins, before any shell tab.
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        assert_eq!(s.tabs[2].id(), OFFLOAD_SERVER_TAB_ID);
        // Non-closable: builtin flag forced on.
        assert!(s.tabs[2].builtin());
    }

    #[test]
    fn integrity_removes_offload_tab_when_disabled() {
        let mut s = base_test_settings();
        s.offload.enabled = true;
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == OFFLOAD_SERVER_TAB_ID));
        // Disable and re-run: the tab is pruned.
        s.offload.enabled = false;
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert!(s.tabs.iter().all(|t| t.id() != OFFLOAD_SERVER_TAB_ID));
    }

    #[test]
    fn reconcile_reserved_tabs_materializes_and_removes_both_live() {
        let mut s = base_test_settings();
        s.offload.enabled = true;
        s.graph.enabled = true;
        // The live toggle path uses reconcile_reserved_tabs (not the full
        // integrity pass) to materialize both reserved tabs at once.
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().any(|t| t.id() == OFFLOAD_SERVER_TAB_ID));
        assert!(s.tabs.iter().any(|t| t.id() == GRAPH_MONITOR_TAB_ID));
        // Idempotent: no flag change → no tab change.
        assert!(!reconcile_reserved_tabs(&mut s));
        // Disabling both prunes both.
        s.offload.enabled = false;
        s.graph.enabled = false;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().all(|t| t.id() != OFFLOAD_SERVER_TAB_ID));
        assert!(s.tabs.iter().all(|t| t.id() != GRAPH_MONITOR_TAB_ID));
    }

    #[test]
    fn integrity_materializes_workbench_tab_by_default() {
        // `workbench.enabled` defaults true, so a fresh install's tab list
        // includes the Workbench tab without anyone touching the flag.
        let mut s = Settings::default();
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == WORKBENCH_TAB_ID));
        let entry = s.tabs.iter().find(|t| t.id() == WORKBENCH_TAB_ID).unwrap();
        assert!(entry.builtin());
    }

    #[test]
    fn integrity_removes_workbench_tab_when_disabled() {
        let mut s = Settings::default();
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == WORKBENCH_TAB_ID));
        s.workbench.enabled = false;
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert!(s.tabs.iter().all(|t| t.id() != WORKBENCH_TAB_ID));
    }

    #[test]
    fn workbench_tab_lands_after_graph_monitor_tab() {
        // Ordering: AI builtins, then Offload Server, then Code Graph
        // monitor, then Workbench, then user shells — mirrors the other two
        // reserved feature tabs' contiguous-leftmost placement.
        let mut s = Settings::default();
        s.offload.enabled = true;
        s.graph.enabled = true;
        integrity_check(&mut s);
        let offload_pos = s.tabs.iter().position(|t| t.id() == OFFLOAD_SERVER_TAB_ID).unwrap();
        let graph_pos = s.tabs.iter().position(|t| t.id() == GRAPH_MONITOR_TAB_ID).unwrap();
        let workbench_pos = s.tabs.iter().position(|t| t.id() == WORKBENCH_TAB_ID).unwrap();
        assert!(offload_pos < graph_pos);
        assert!(graph_pos < workbench_pos);
    }

    #[test]
    fn reconcile_reserved_tabs_covers_workbench_live_toggle() {
        // Start from an already-materialized tab (as a loaded settings file
        // would have, `workbench.enabled` defaulting true) and disable live.
        let mut s = Settings::default();
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == WORKBENCH_TAB_ID));

        s.workbench.enabled = false;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().all(|t| t.id() != WORKBENCH_TAB_ID));
        // Idempotent while it stays disabled.
        assert!(!reconcile_reserved_tabs(&mut s));

        // Re-enabling live materializes it again.
        s.workbench.enabled = true;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().any(|t| t.id() == WORKBENCH_TAB_ID));
    }

    #[test]
    fn integrity_materializes_tool_activity_tab_by_default() {
        // `ui.tool_activity_tab` defaults true, so a fresh install's tab list
        // includes the Tool Activity tab without anyone touching the flag —
        // positioned after the other reserved feature tabs (here: Workbench,
        // the only other default-on one).
        let mut s = Settings::default();
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == TOOL_ACTIVITY_TAB_ID));
        let workbench_pos = s.tabs.iter().position(|t| t.id() == WORKBENCH_TAB_ID).unwrap();
        let tool_activity_pos =
            s.tabs.iter().position(|t| t.id() == TOOL_ACTIVITY_TAB_ID).unwrap();
        assert!(workbench_pos < tool_activity_pos);
    }

    #[test]
    fn reconcile_reserved_tabs_covers_tool_activity_live_toggle() {
        // Start from an already-materialized tab (`ui.tool_activity_tab`
        // defaulting true) and disable live — same shape as the Workbench
        // live-toggle test above.
        let mut s = Settings::default();
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == TOOL_ACTIVITY_TAB_ID));

        s.ui.tool_activity_tab = false;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().all(|t| t.id() != TOOL_ACTIVITY_TAB_ID));
        // Idempotent while it stays disabled.
        assert!(!reconcile_reserved_tabs(&mut s));

        // Re-enabling live materializes it again.
        s.ui.tool_activity_tab = true;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().any(|t| t.id() == TOOL_ACTIVITY_TAB_ID));
    }

    #[test]
    fn reconcile_audit_tools_appends_missing_quality_tools() {
        use crate::settings::schema::{AuditToolConfig, AuditToolId};
        // A v0.43/v0.44 persisted config: only the three Security tools, one of
        // them customized (disabled + custom path + extra args + timeout).
        let mut s = base_test_settings();
        s.code_audit.tools = vec![
            AuditToolConfig {
                id: AuditToolId::OsvScanner,
                enabled: false,
                path: r"C:\tools\osv.exe".to_string(),
                extra_args: vec!["--offline".to_string()],
                timeout_secs: Some(42),
            },
            AuditToolConfig {
                id: AuditToolId::Gitleaks,
                enabled: true,
                path: String::new(),
                extra_args: vec![],
                timeout_secs: None,
            },
            AuditToolConfig {
                id: AuditToolId::Semgrep,
                enabled: true,
                path: String::new(),
                extra_args: vec![],
                timeout_secs: None,
            },
        ];

        let changed = reconcile_audit_tools(&mut s);
        assert!(changed);
        let tools = &s.code_audit.tools;
        assert_eq!(tools.len(), 14);

        // The three Security entries are preserved verbatim, in order — the
        // customized osv-scanner survives untouched.
        assert_eq!(tools[0].id, AuditToolId::OsvScanner);
        assert!(!tools[0].enabled);
        assert_eq!(tools[0].path, r"C:\tools\osv.exe");
        assert_eq!(tools[0].extra_args, vec!["--offline".to_string()]);
        assert_eq!(tools[0].timeout_secs, Some(42));
        assert_eq!(tools[1].id, AuditToolId::Gitleaks);
        assert_eq!(tools[2].id, AuditToolId::Semgrep);

        // The eleven Quality ids are appended with correct enabled defaults:
        // enabled except dotnet-analyzers and semgrep-quality.
        let by_id = |id| tools.iter().find(|t| t.id == id).unwrap();
        for id in [
            AuditToolId::Oxlint,
            AuditToolId::GolangciLint,
            AuditToolId::Ruff,
            AuditToolId::Cppcheck,
            AuditToolId::Typos,
            AuditToolId::Eslint,
            AuditToolId::Pmd,
            AuditToolId::Knip,
            AuditToolId::CargoMachete,
        ] {
            assert!(by_id(id).enabled, "{id:?} enabled by default");
            assert!(by_id(id).path.is_empty(), "{id:?} no path override");
            assert!(by_id(id).timeout_secs.is_none(), "{id:?} global timeout");
        }
        assert!(!by_id(AuditToolId::DotnetAnalyzers).enabled);
        assert!(!by_id(AuditToolId::SemgrepQuality).enabled);
    }

    #[test]
    fn reconcile_audit_tools_leaves_full_config_untouched() {
        // A fresh install already carries all fourteen tools — nothing to add.
        let mut s = base_test_settings();
        let before = s.code_audit.tools.clone();
        assert_eq!(before.len(), 14);
        assert!(!reconcile_audit_tools(&mut s));
        assert_eq!(s.code_audit.tools, before);
    }

    #[test]
    fn reconcile_audit_tools_is_idempotent() {
        use crate::settings::schema::{AuditToolConfig, AuditToolId};
        let mut s = base_test_settings();
        s.code_audit.tools = vec![AuditToolConfig {
            id: AuditToolId::Gitleaks,
            enabled: true,
            path: String::new(),
            extra_args: vec![],
            timeout_secs: None,
        }];
        assert!(reconcile_audit_tools(&mut s));
        let after_first = s.code_audit.tools.clone();
        assert_eq!(after_first.len(), 14);
        // A second pass finds every id present and changes nothing.
        assert!(!reconcile_audit_tools(&mut s));
        assert_eq!(s.code_audit.tools, after_first);
    }

    #[test]
    fn integrity_reconciles_pre_v25_audit_tools() {
        use crate::settings::schema::{AuditToolConfig, AuditToolId};
        // The load-path integrity pass performs the same reconcile, so an
        // upgraded install gains the Quality tools on first load.
        let mut s = base_test_settings();
        s.code_audit.tools = vec![
            AuditToolConfig {
                id: AuditToolId::OsvScanner,
                enabled: true,
                path: String::new(),
                extra_args: vec![],
                timeout_secs: None,
            },
            AuditToolConfig {
                id: AuditToolId::Gitleaks,
                enabled: true,
                path: String::new(),
                extra_args: vec![],
                timeout_secs: None,
            },
            AuditToolConfig {
                id: AuditToolId::Semgrep,
                enabled: true,
                path: String::new(),
                extra_args: vec![],
                timeout_secs: None,
            },
        ];
        integrity_check(&mut s);
        assert_eq!(s.code_audit.tools.len(), 14);
    }

    #[test]
    fn integrity_inserts_opencode_between_claude_local_and_user_shell() {
        // User has [claude, claude-local, shell-foo] and now enables
        // opencode. The new tab should land at index 2 (after claude-local,
        // before the shell), not at the end.
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![
            AiTabId::Claude,
            AiTabId::ClaudeLocal,
            AiTabId::OpenCode,
        ];
        integrity_check(&mut s);
        // Insert a user shell tab to simulate the existing layout.
        s.tabs.push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
            id: "shell-foo".to_string(),
            builtin: false,
            name: "Foo".to_string(),
            command: "/bin/bash".to_string(),
            args: vec!["-i".to_string()],
            cwd: None,
            env: Default::default(),
            notifications: Default::default(),
            theme_override: None,
            background_override: None,
        }));
        // Drop opencode, then re-add via integrity.
        s.tabs.retain(|t| t.id() != OPENCODE_TAB_ID);
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        assert_eq!(s.tabs[2].id(), OPENCODE_TAB_ID);
        assert_eq!(s.tabs[3].id(), "shell-foo");
    }

    #[test]
    fn integrity_drops_disabled_ai_tab() {
        // Loading a file where the setting and tabs disagree (e.g. a
        // hand-edit, or post-migration drift) reconciles to the setting.
        let mut s = base_test_settings();
        let _shell = fake_default_shell();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        integrity_check(&mut s);
        assert_eq!(s.tabs.len(), 2);

        s.enabled_ai_tabs = vec![AiTabId::Claude];
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
    }

    #[test]
    fn integrity_repairs_empty_enabled_ai_tabs() {
        // A hand-edited file with `enabled_ai_tabs: []` is invalid;
        // integrity forces it back to [claude] so the user always boots
        // with at least one AI tab.
        let mut s = base_test_settings();
        s.enabled_ai_tabs = Vec::new();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.enabled_ai_tabs, vec![AiTabId::Claude]);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
    }

    #[test]
    fn integrity_does_not_restore_shell_default_1() {
        // Closing shell-default-1 must persist across launches: the
        // integrity check should leave it absent.
        let mut s = base_test_settings();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        assert!(s
            .tabs
            .iter()
            .all(|t| t.id() != SHELL_DEFAULT_TAB_ID));
    }

    #[test]
    fn integrity_demotes_legacy_shell_default_1_to_non_builtin() {
        // Older settings files persisted shell-default-1 with builtin: true.
        // Loading those files must demote the entry so the close button
        // works.
        let mut s = base_test_settings();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        // Insert a legacy-shaped shell-default-1 with builtin: true.
        s.tabs.push(TabConfig::Shell(
            crate::settings::schema::ShellTabConfig {
                id: SHELL_DEFAULT_TAB_ID.to_string(),
                builtin: true,
                name: "Shell 1".to_string(),
                command: "/bin/bash".to_string(),
                args: vec!["-i".to_string()],
                cwd: None,
                env: Default::default(),
                notifications: Default::default(),
                theme_override: None,
                background_override: None,
            },
        ));
        let changed = integrity_check(&mut s);
        assert!(changed);
        let entry = s
            .tabs
            .iter()
            .find(|t| t.id() == SHELL_DEFAULT_TAB_ID)
            .expect("shell-default-1 still present");
        assert!(!entry.builtin());
    }

    #[test]
    fn integrity_forces_builtin_true_on_ai_builtins() {
        let mut s = base_test_settings();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        // Tamper: flip claude's builtin to false.
        if let TabConfig::AiTool(c) = &mut s.tabs[0] {
            c.builtin = false;
        }
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert!(s.tabs[0].builtin());
    }

    #[test]
    fn integrity_preserves_user_tabs() {
        let mut s = base_test_settings();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        // Insert a user shell tab.
        s.tabs.push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
            id: "shell-user-1".to_string(),
            builtin: false,
            name: "Build Watch".to_string(),
            command: "/bin/bash".to_string(),
            args: vec!["-i".to_string()],
            cwd: None,
            env: Default::default(),
            notifications: Default::default(),
            theme_override: None,
            background_override: None,
        }));
        let user_pos_before = s.tabs.len() - 1;

        // Delete claude — integrity should restore it without disturbing
        // the user tab's relative position.
        s.tabs.retain(|t| t.id() != CLAUDE_TAB_ID);
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        let user_pos_after = s
            .tabs
            .iter()
            .position(|t| t.id() == "shell-user-1")
            .unwrap();
        // User tab should still be at the end.
        assert_eq!(user_pos_after, s.tabs.len() - 1);
        let _ = user_pos_before;
    }

    #[test]
    fn v1_2_round_trip() {
        let _shell = fake_default_shell();
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        integrity_check(&mut s);
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.tabs.len(), 2);
        assert_eq!(parsed.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(parsed.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_corrects_use_local_provider_on_reserved_ai_tabs() {
        // A hand-edit must not be able to silently flip the
        // subscription Claude tab into local-LLM mode (or vice versa).
        // Enable both so the check has both AI tabs to validate.
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        let _shell = fake_default_shell();
        integrity_check(&mut s);

        // Tamper: flip claude → local, claude-local → not local.
        if let TabConfig::AiTool(c) = &mut s.tabs[0] {
            c.use_local_provider = true;
        }
        if let TabConfig::AiTool(c) = &mut s.tabs[1] {
            c.use_local_provider = false;
        }

        let changed = integrity_check(&mut s);
        assert!(changed);
        if let TabConfig::AiTool(c) = &s.tabs[0] {
            assert!(!c.use_local_provider, "claude must have use_local_provider=false");
        }
        if let TabConfig::AiTool(c) = &s.tabs[1] {
            assert!(c.use_local_provider, "claude-local must have use_local_provider=true");
        }
    }

    #[test]
    fn integrity_corrects_use_local_provider_on_opencode() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::OpenCode];
        integrity_check(&mut s);
        // Tamper: opencode → local (it has no local variant; canonical is false).
        if let TabConfig::AiTool(c) = s.tabs.iter_mut().find(|t| t.id() == OPENCODE_TAB_ID).unwrap() {
            c.use_local_provider = true;
        }
        let changed = integrity_check(&mut s);
        assert!(changed);
        if let TabConfig::AiTool(c) = s.tabs.iter().find(|t| t.id() == OPENCODE_TAB_ID).unwrap() {
            assert!(!c.use_local_provider, "opencode must have use_local_provider=false");
        }
    }

    #[test]
    fn ui_theme_round_trip_and_default() {
        // Default file has ui.theme = "tui-blue" (new installs land here).
        let s = Settings::default();
        assert_eq!(s.ui.theme, "tui-blue");

        // Round-trip preserves a hand-edited value (here: a user who
        // switched to tui-grey or set a future theme).
        let mut s = Settings::default();
        s.ui.theme = "tui-grey".to_string();
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme, "tui-grey");

        // A v1.3 file without the `ui` field still parses (serde(default)).
        let v1_3_json = r#"{"tabs":[]}"#;
        let parsed: Settings = serde_json::from_str(v1_3_json).unwrap();
        assert_eq!(parsed.ui.theme, "tui-blue");
    }

    // --- Layered config (global + custom overlay) -----------------------

    #[test]
    fn deep_merge_object_keys() {
        let mut base = serde_json::json!({
            "a": 1,
            "nested": { "x": 1, "y": 2 },
        });
        let overlay = serde_json::json!({
            "a": 99,
            "nested": { "y": 22, "z": 33 },
            "new_key": "added",
        });
        deep_merge(&mut base, overlay);
        assert_eq!(
            base,
            serde_json::json!({
                "a": 99,
                "nested": { "x": 1, "y": 22, "z": 33 },
                "new_key": "added",
            })
        );
    }

    #[test]
    fn deep_merge_arrays_replace_wholesale() {
        let mut base = serde_json::json!({ "tabs": [1, 2, 3] });
        let overlay = serde_json::json!({ "tabs": [9] });
        deep_merge(&mut base, overlay);
        assert_eq!(base, serde_json::json!({ "tabs": [9] }));
    }

    #[test]
    fn diff_identical_returns_none() {
        let v = serde_json::json!({ "a": 1, "b": [1, 2] });
        assert!(diff(&v, &v).is_none());
    }

    #[test]
    fn diff_drops_matching_keys() {
        let current = serde_json::json!({
            "a": 1,
            "nested": { "x": 1, "y": 99 },
        });
        let baseline = serde_json::json!({
            "a": 1,
            "nested": { "x": 1, "y": 2 },
        });
        let d = diff(&current, &baseline).unwrap();
        assert_eq!(d, serde_json::json!({ "nested": { "y": 99 } }));
    }

    #[test]
    fn diff_emits_null_for_keys_only_in_baseline() {
        // Models a removal in the overlay so the reverse merge can
        // reconstruct it. (Not common in our typed Settings — every field
        // has a default — but the diff/merge pair must round-trip
        // generic JSON objects cleanly.)
        let current = serde_json::json!({ "a": 1 });
        let baseline = serde_json::json!({ "a": 1, "b": 2 });
        let d = diff(&current, &baseline).unwrap();
        assert_eq!(d, serde_json::json!({ "b": null }));
    }

    #[test]
    fn diff_arrays_replace_whole() {
        let current = serde_json::json!({ "tabs": [1, 2, 3] });
        let baseline = serde_json::json!({ "tabs": [1, 2] });
        let d = diff(&current, &baseline).unwrap();
        assert_eq!(d, serde_json::json!({ "tabs": [1, 2, 3] }));
    }

    #[test]
    fn merge_then_diff_round_trip_typed_settings() {
        // Take default Settings as global, mutate one nested field, diff
        // it, and verify that re-applying the diff to global recovers the
        // mutated state.
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        let mut customized = global.clone();
        customized.ui.theme = "future-light".to_string();

        let g_value = serde_json::to_value(&global).unwrap();
        let c_value = serde_json::to_value(&customized).unwrap();
        let delta = diff(&c_value, &g_value).expect("non-empty diff");

        // The delta should be tiny — just the ui.theme branch.
        assert_eq!(delta, serde_json::json!({ "ui": { "theme": "future-light" } }));

        // Reverse: apply delta to global, deserialize, confirm we get
        // `customized` back.
        let mut reapplied = g_value.clone();
        deep_merge(&mut reapplied, delta);
        let recovered: Settings = serde_json::from_value(reapplied).unwrap();
        assert_eq!(recovered.ui.theme, "future-light");
    }

    #[test]
    fn stamp_avatar_paths_uses_files_present_in_dir() {
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        // Stage two of the five state videos plus the transition; the
        // remaining three should be left untouched (None / embedded URL).
        for f in ["Idle.mp4", "Speaking.mp4", "Transition.mp4"] {
            fs::write(dir.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        // Sanity: defaults start with images None and transition pointing
        // at the embedded `/avatar/...` URL.
        assert!(s.avatar.images.idle.is_none());
        assert_eq!(
            s.avatar.transition.path.as_deref().map(|p| p.to_string_lossy().to_string()),
            Some("/avatar/Transition.mp4".to_string())
        );

        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(s.avatar.images.idle.as_deref(), Some(dir.join("Idle.mp4").as_path()));
        assert_eq!(s.avatar.images.speaking.as_deref(), Some(dir.join("Speaking.mp4").as_path()));
        assert!(s.avatar.images.listening.is_none(), "missing files should not be stamped");
        assert!(s.avatar.images.thinking.is_none());
        assert!(s.avatar.images.error.is_none());
        assert_eq!(s.avatar.transition.path.as_deref(), Some(dir.join("Transition.mp4").as_path()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_prefers_theme_subfolder() {
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_themed_{}", uuid::Uuid::new_v4()));
        let modern = dir.join("modern-dark");
        let tui_yellow = dir.join("tui-yellow");
        fs::create_dir_all(&modern).unwrap();
        fs::create_dir_all(&tui_yellow).unwrap();

        // Stage the same files in both theme folders so we can prove the
        // active theme drives the selection rather than alphabetical luck.
        for f in ["Idle.mp4", "Speaking.mp4", "Transition.mp4"] {
            fs::write(modern.join(f), b"").unwrap();
            fs::write(tui_yellow.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(s.avatar.images.idle.as_deref(), Some(tui_yellow.join("Idle.mp4").as_path()));
        assert_eq!(
            s.avatar.images.speaking.as_deref(),
            Some(tui_yellow.join("Speaking.mp4").as_path()),
        );
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(tui_yellow.join("Transition.mp4").as_path()),
        );

        // Switching themes restamps from the other folder.
        let mut s2 = Settings::default();
        s2.ui.theme = "modern-dark".to_string();
        stamp_avatar_paths_from(&mut s2, &dir);
        assert_eq!(
            s2.avatar.images.idle.as_deref(),
            Some(modern.join("Idle.mp4").as_path()),
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_falls_back_to_flat_layout() {
        // Legacy zips (pre per-theme split) staged the videos at the top
        // of `avatars/`. Verify those still get picked up when the active
        // theme's subfolder is missing.
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_flat_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        for f in ["Idle.mp4", "Transition.mp4"] {
            fs::write(dir.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string(); // tui-yellow/ subfolder does not exist
        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(s.avatar.images.idle.as_deref(), Some(dir.join("Idle.mp4").as_path()));
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(dir.join("Transition.mp4").as_path()),
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_noop_when_dir_empty() {
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_empty_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut s = Settings::default();
        let before_transition = s.avatar.transition.path.clone();
        stamp_avatar_paths_from(&mut s, &dir);

        assert!(s.avatar.images.idle.is_none());
        assert!(s.avatar.images.listening.is_none());
        assert!(s.avatar.images.thinking.is_none());
        assert!(s.avatar.images.speaking.is_none());
        assert!(s.avatar.images.error.is_none());
        assert_eq!(s.avatar.transition.path, before_transition);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_preserves_user_override_outside_dir() {
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_ovr_{}", uuid::Uuid::new_v4()));
        let theme = dir.join("tui-yellow");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("Idle.mp4"), b"").unwrap();

        // A genuine override the user picked from elsewhere on disk.
        let custom = std::env::temp_dir()
            .join(format!("cimp_custom_{}.mp4", uuid::Uuid::new_v4()));
        fs::write(&custom, b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        s.avatar.images.idle = Some(custom.clone());
        stamp_avatar_paths_from(&mut s, &dir);

        // The override (outside `dir`) survives; bundled fields are stamped.
        assert_eq!(s.avatar.images.idle.as_deref(), Some(custom.as_path()));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&custom);
    }

    #[test]
    fn stamp_avatar_paths_repoints_on_theme_switch() {
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_switch_{}", uuid::Uuid::new_v4()));
        let yellow = dir.join("tui-yellow");
        let purple = dir.join("tui-purple");
        fs::create_dir_all(&yellow).unwrap();
        fs::create_dir_all(&purple).unwrap();
        fs::write(yellow.join("Idle.mp4"), b"").unwrap();
        fs::write(purple.join("Idle.mp4"), b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(s.avatar.images.idle.as_deref(), Some(yellow.join("Idle.mp4").as_path()));

        // The previously-stamped path (inside `dir`) is re-pointed to the new
        // theme, NOT mistaken for a user override. This is the actual bug fix.
        s.ui.theme = "tui-purple".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(s.avatar.images.idle.as_deref(), Some(purple.join("Idle.mp4").as_path()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_resets_when_new_theme_has_no_files() {
        let dir = std::env::temp_dir()
            .join(format!("cimp_avatars_reset_{}", uuid::Uuid::new_v4()));
        let yellow = dir.join("tui-yellow");
        fs::create_dir_all(&yellow).unwrap();
        fs::write(yellow.join("Idle.mp4"), b"").unwrap();
        fs::write(yellow.join("Transition.mp4"), b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(s.avatar.images.idle.as_deref(), Some(yellow.join("Idle.mp4").as_path()));
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(yellow.join("Transition.mp4").as_path()),
        );

        // Switch to a theme with no on-disk folder: the image resets to None
        // (frontend uses the embedded bundled video) and the transition
        // reverts to the redirectable `/avatar/` URL — never a stale
        // tui-yellow path, never a disabling None.
        s.ui.theme = "modern-dark".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert!(s.avatar.images.idle.is_none());
        assert_eq!(
            s.avatar.transition.path.as_deref().map(|p| p.to_string_lossy().to_string()),
            Some("/avatar/Transition.mp4".to_string())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_overlay_when_diff_nonempty_and_removes_when_empty() {
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        // Use a unique subdir under the system temp root so parallel test
        // runs don't collide. Cleaned up at the end of the test.
        let dir = std::env::temp_dir()
            .join(format!("cimp_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        // The overlay now lives inside the project's `.cimp/` dir.
        let overlay = custom_path(&dir);

        // Customized: should write a non-empty overlay.
        let mut customized = global.clone();
        customized.ui.theme = "future-light".to_string();
        save(&customized, &dir, &global).unwrap();
        assert!(overlay.exists());
        let text = fs::read_to_string(&overlay).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "ui": { "theme": "future-light" } }));

        // Reverted to identical: should remove the overlay.
        save(&global, &dir, &global).unwrap();
        assert!(!overlay.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checks_suggestion_dismissal_persists_through_the_overlay() {
        // V22 Phase D: the nudge dismissal (and the auto-configure toggle) are
        // ordinary per-project fields — they ride the `.cimp/config.json`
        // overlay diff and reconstitute on load. A pre-Phase-D config (neither
        // key) defaults both to false.
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        let dir = std::env::temp_dir().join(format!("cimp_checks_dismiss_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let overlay = custom_path(&dir);

        let mut customized = global.clone();
        customized.checks_suggestion_dismissed = true;
        customized.checks_auto_configure = true;
        save(&customized, &dir, &global).unwrap();

        // The diff carries both fields (they differ from the default baseline).
        let text = fs::read_to_string(&overlay).unwrap();
        assert!(text.contains("checks_suggestion_dismissed"), "overlay: {text}");
        assert!(text.contains("checks_auto_configure"), "overlay: {text}");

        // Reconstitute: merge the overlay back onto the default baseline.
        let mut merged = serde_json::to_value(&global).unwrap();
        let overlay_val: Value = serde_json::from_str(&text).unwrap();
        deep_merge(&mut merged, overlay_val);
        let loaded: Settings = serde_json::from_value(merged).unwrap();
        assert!(loaded.checks_suggestion_dismissed, "dismissal survives a save→merge roundtrip");
        assert!(loaded.checks_auto_configure);

        // A config predating Phase D (neither key) defaults both to false.
        let old: Settings = serde_json::from_str(r#"{"schema_version": 21}"#).unwrap();
        assert!(!old.checks_suggestion_dismissed);
        assert!(!old.checks_auto_configure);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_overlay_inside_cimp_dir() {
        // The per-folder overlay must land at `<cwd>/.cimp/config.json`, not
        // the pre-consolidation loose `<cwd>/.cimp.custom.config.json`.
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        let dir = std::env::temp_dir().join(format!("cimp_loc_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut customized = global.clone();
        customized.ui.theme = "future-light".to_string();
        save(&customized, &dir, &global).unwrap();

        assert!(dir.join(".cimp").join("config.json").exists());
        assert!(!dir.join(".cimp.custom.config.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_overlay_moves_loose_file_into_cimp_dir() {
        let dir = std::env::temp_dir().join(format!("cimp_mig_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        // Seed a pre-consolidation loose overlay.
        let legacy = dir.join(".cimp.custom.config.json");
        fs::write(&legacy, r#"{"ui":{"theme":"future-light"}}"#).unwrap();

        migrate_legacy_overlay(&dir);

        let canonical = dir.join(".cimp").join("config.json");
        assert!(canonical.exists(), "legacy overlay should be moved into .cimp/");
        assert!(!legacy.exists(), "legacy overlay should be gone after the move");
        assert_eq!(
            fs::read_to_string(&canonical).unwrap(),
            r#"{"ui":{"theme":"future-light"}}"#
        );
        // The resolver now points at the canonical file.
        assert_eq!(overlay_read_path(&dir), canonical);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_overlay_noop_when_canonical_exists() {
        // A newer canonical overlay wins; the legacy loose file is left as-is
        // (never overwrites the canonical, never silently reappears).
        let dir = std::env::temp_dir().join(format!("cimp_mig2_{}", uuid::Uuid::new_v4()));
        let cimp = dir.join(".cimp");
        fs::create_dir_all(&cimp).unwrap();
        fs::write(cimp.join("config.json"), r#"{"ui":{"theme":"canonical"}}"#).unwrap();
        let legacy = dir.join(".cimp.custom.config.json");
        fs::write(&legacy, r#"{"ui":{"theme":"legacy"}}"#).unwrap();

        migrate_legacy_overlay(&dir);

        assert!(legacy.exists(), "legacy untouched when canonical present");
        assert_eq!(
            fs::read_to_string(cimp.join("config.json")).unwrap(),
            r#"{"ui":{"theme":"canonical"}}"#
        );
        assert_eq!(overlay_read_path(&dir), cimp.join("config.json"));

        let _ = fs::remove_dir_all(&dir);
    }

    // --- V14 Phase A: prompt library persistence ------------------------

    #[test]
    fn seed_prompt_templates_if_needed_seeds_once() {
        let mut s = Settings::default();
        assert!(!s.templates_seeded);
        assert!(s.prompt_templates.is_empty());

        let seeded_first = seed_prompt_templates_if_needed(&mut s);
        assert!(seeded_first);
        assert!(s.templates_seeded);
        assert_eq!(s.prompt_templates.len(), 4);

        // A deletion the user made must stick: a second call is a no-op even
        // though the list is now empty again.
        s.prompt_templates.clear();
        let seeded_second = seed_prompt_templates_if_needed(&mut s);
        assert!(!seeded_second, "seeding must not re-fire once templates_seeded is true");
        assert!(s.prompt_templates.is_empty(), "deleted starters must stay deleted");
    }

    #[test]
    fn write_then_read_global_prompt_templates_round_trips() {
        let dir = std::env::temp_dir().join(format!("cimp_tpl_global_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let templates = vec![
            PromptTemplate { name: "a".to_string(), body: "body-a".to_string() },
            PromptTemplate { name: "b".to_string(), body: "body-b".to_string() },
        ];
        write_prompt_templates_to(&path, templates.clone()).unwrap();

        let read_back = read_prompt_templates_from(&path);
        assert_eq!(read_back, templates);

        // templates_seeded is forced true by an explicit write, so a later
        // seed-if-needed pass is a no-op (a user-authored list is never
        // clobbered by the starter set).
        let mut settings: Settings =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(settings.templates_seeded);
        assert!(!seed_prompt_templates_if_needed(&mut settings));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_global_prompt_templates_preserves_other_fields() {
        let dir = std::env::temp_dir().join(format!("cimp_tpl_preserve_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut initial = Settings::default();
        initial.ui.theme = "future-light".to_string();
        save_to(&path, &initial).unwrap();

        write_prompt_templates_to(
            &path,
            vec![PromptTemplate { name: "a".to_string(), body: "x".to_string() }],
        )
        .unwrap();

        let after: Settings = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after.ui.theme, "future-light", "unrelated field must survive the R-M-W");
        assert_eq!(after.prompt_templates.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_then_read_global_llm_pricing_round_trips() {
        let dir = std::env::temp_dir().join(format!("cimp_price_global_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A missing file reads as the seeded defaults, never empty.
        let seeded = read_llm_pricing_from(&path);
        assert_eq!(seeded, crate::settings::default_llm_pricing());
        assert!(!seeded.is_empty());

        let pricing = vec![LlmPricingModel {
            provider: "Anthropic".to_string(),
            model: "Claude Opus 4.8".to_string(),
            model_prefix: "claude-opus-4-8".to_string(),
            input: 5.0,
            cache_write: 10.0,
            cache_read: 0.5,
            output: 25.0,
        }];
        write_llm_pricing_to(&path, pricing.clone()).unwrap();
        assert_eq!(read_llm_pricing_from(&path), pricing);

        // Deleting every row must stick: an explicit empty write reads back
        // empty (the key is present in the file), not as re-seeded defaults.
        write_llm_pricing_to(&path, Vec::new()).unwrap();
        assert!(read_llm_pricing_from(&path).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_global_llm_pricing_preserves_other_fields() {
        let dir = std::env::temp_dir().join(format!("cimp_price_preserve_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut initial = Settings::default();
        initial.ui.theme = "future-light".to_string();
        save_to(&path, &initial).unwrap();

        write_llm_pricing_to(&path, Vec::new()).unwrap();

        let after: Settings = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after.ui.theme, "future-light", "unrelated field must survive the R-M-W");
        assert!(after.llm_pricing.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_project_prompt_templates_reads_the_overlays_own_array() {
        let dir = std::env::temp_dir().join(format!("cimp_tpl_project_{}", uuid::Uuid::new_v4()));
        let cimp = dir.join(".cimp");
        fs::create_dir_all(&cimp).unwrap();
        fs::write(
            cimp.join("config.json"),
            r#"{"prompt_templates":[{"name":"p1","body":"project body"}]}"#,
        )
        .unwrap();

        let templates = read_project_prompt_templates(&dir);
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "p1");
        assert_eq!(templates[0].body, "project body");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_project_prompt_templates_empty_when_overlay_absent_or_key_missing() {
        let dir = std::env::temp_dir().join(format!("cimp_tpl_project_absent_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        // No overlay file at all.
        assert!(read_project_prompt_templates(&dir).is_empty());

        // Overlay exists but carries no `prompt_templates` key.
        let cimp = dir.join(".cimp");
        fs::create_dir_all(&cimp).unwrap();
        fs::write(cimp.join("config.json"), r#"{"ui":{"theme":"x"}}"#).unwrap();
        assert!(read_project_prompt_templates(&dir).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}

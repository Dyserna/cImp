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
// The per-project cImp data directory. Holds the per-folder settings overlay
// (`config.json`) and the code-graph store (`graph.db`). It was declared here
// and copied into two other modules until the V42 review folded the three into
// one — see [`crate::fsutil::CIMP_DIR_NAME`], which also carries the reason it
// is not derived from `graph.db_subdir`.
use crate::fsutil::CIMP_DIR_NAME;
use crate::settings::migration;
use crate::settings::schema::{
    default_ai_tab, default_events_tab, default_graph_monitor_tab,
    default_shell_1_tab, default_tool_activity_tab, default_workbench_tab,
    starter_prompt_templates, AiTabId, HarnessVersions, LayoutNodePersisted, LlmPricingModel,
    McpCategory, McpServerConfig, PromptTemplate, RemoteBackendTemplate, ServerCommandTemplate,
    Settings, TabConfig,
    CODE_AUDIT_TAB_ID, CODE_QUALITY_TAB_ID, EVENTS_TAB_ID, GRAPH_MONITOR_TAB_ID,
    GRAPH_VIEW_TAB_ID, OFFLOAD_SERVER_TAB_ID,
    SHELL_DEFAULT_TAB_ID, TOOL_ACTIVITY_TAB_ID, WORKBENCH_TAB_ID,
};
use crate::pricing::{pricing_rows_since, PRICING_GENERATION};
use crate::settings::write_atomic;
use crate::shell::ShellSpec;

const GLOBAL_FILE_NAME: &str = "settings.json";
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
/// **Migration runs on the overlay too, since V40 Phase I** (issue #107 item
/// 5) — from the version the overlay states, or (for one written before the
/// stamp existed) the version the global file stated beside it. See
/// `migration::migrate_overlay` for why entering the cascade blind was the
/// thing that was wrong, not migrating the overlay at all.
pub fn load(default_shell: &ShellSpec, launch_cwd: &Path) -> LoadOutcome {
    // 1. Load and migrate the global baseline. After this `global` is in
    //    the current schema shape; a v1.x file on disk has been backed up
    //    next to the global path and rewritten. `global_stated` is the version
    //    the file claimed BEFORE that, which step 2a needs.
    let (mut global, global_stated) = load_global(default_shell);

    // 2. Load the overlay (if any).
    //    First fold any pre-consolidation loose overlay into `.cimp/`, then
    //    read from the resolved location (canonical `.cimp/config.json`, or the
    //    legacy file if the move couldn't happen).
    migrate_legacy_overlay(launch_cwd);
    let overlay_path = overlay_read_path(launch_cwd);
    let overlay_value = read_overlay(&overlay_path, true).map(|mut v| {
        // 2a. **Migrate it, before anything reads its shape** (V40 Phase I).
        //     Until Phase I this was skipped, for two reasons that were both
        //     about entering the cascade BLIND: the presence-archaeology
        //     detectors (deleted by V42 R9) keyed off top-level keys a partial
        //     diff legitimately lacks, and a value with no version re-migrates
        //     every launch, growing `.bak` files without bound. `migrate_overlay`
        //     is told the version, refuses to start below the migration floor,
        //     and writes no file at all — so neither reason survives, and the gap
        //     they were covering does not: a project that set
        //     `claude_local.base_url` before schema 36 kept a top-level
        //     `claude_local` block that reached nothing after the global moved
        //     the field, with the file still on disk saying otherwise.
        //
        //     The version comes from the overlay's own `schema_version` stamp
        //     (written by `save` since Phase I) and falls back to what the
        //     global file stated: an overlay beside a v35 global was written
        //     against a v35 baseline. Neither present ⇒ the overlay is current,
        //     which is what every reader assumed before Phase I anyway.
        //     The stamp is stripped inside `migrate_overlay`; it must never
        //     reach the merge, or an old overlay would pin the merged
        //     `schema_version` below the global's.
        let from = migration::stated_schema_version(&v)
            .or(global_stated)
            .unwrap_or(crate::settings::schema::CURRENT_SCHEMA_VERSION as u64);
        if migration::migrate_overlay(&mut v, from) {
            tracing::info!(
                path = %overlay_path.display(),
                from,
                "settings: project overlay migrated in memory (written back on the next save)"
            );
        }
        // Every machine-scope family, in one walk of [`MACHINE_SCOPED`] — drop
        // them before the merge so an overlay contaminated by a pre-guard
        // version can't shadow the global file. The whole-key bans go silently
        // (see `OVERLAY_BANNED_KEYS`); the structured strips NAME what they
        // dropped, because a hand-edited config that sets a plugin's binary path
        // or one of the per-harness fields per repo is a reasonable thing to try
        // and a silent no-op is how that becomes "cImp ignores my config" an
        // hour later. A family added to the table is stripped here without this
        // call site being touched, which is the whole point of the table.
        let dropped = strip_overlay_for_merge(&mut v);
        crate::plugins::events::record_overlay_strip(
            &overlay_path.display().to_string(),
            &dropped,
        );
        v
    });

    // 2b. Promote legacy overlay data into the global baseline — every
    //     `promote` cell of [`MACHINE_SCOPED`]: the audit scanner paths (empty
    //     slots only), the offload template libraries and the MCP registry (new
    //     names only), each described above its own function. Persisted below
    //     via the post-load `save`, which also rewrites the overlay in the
    //     stripped shape.
    let promoted = overlay_value
        .as_ref()
        .is_some_and(|ov| promote_overlay_into_global(&mut global, ov));

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
        // 3b. **After the merge, never before.** Template libraries and the MCP
        //     registry always come from the (post-promotion) global baseline —
        //     an overlay's copies are legacy data, not authority. Every
        //     `enforce` cell of [`MACHINE_SCOPED`].
        enforce_global_machine_scope(&mut merged, &global);
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

    // The merged view goes through the same parse boundary (V40 review M-1):
    // `load_global` normalised the baseline, but a project overlay's
    // `harness.<id>.ext` values are merged in AFTER that and are just as
    // hand-editable. Not folded into `repaired` — a normalisation of the merged
    // value is not a repair of the GLOBAL file, and `load_global` has already
    // healed that one on disk.
    settings.normalize_harness_settings();
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
    // materialized reserved feature tabs, canonical flags) read as user
    // customizations on every save and leak into the portable per-folder
    // overlay. The load-time post-repair save below relies on this too.
    let _ = integrity_check(&mut global);

    if repaired || promoted {
        // Persist the post-repair state back to its source of truth. If a
        // custom overlay was in play, we recompute and rewrite the diff;
        // otherwise we rewrite global. (A path promotion always has an
        // overlay in play; `save` both writes the promoted paths through to
        // the physical global file and rewrites the overlay stripped, so the
        // promotion never re-fires.)
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
        // The same table [`load`] walks, on this leg's cells — and NOT optional
        // here. The children this serves are the Phase C/D consumers that
        // resolve a plugin tool's binary path and enable state, and they run
        // INSIDE the sandbox boundary whose writable area holds this very file;
        // `expose_commands` decides whether `run_command` is advertised, and
        // this reader IS the child that asks. The MCP registry is the one family
        // the two readers handle differently, and the difference lives in the
        // TABLE rather than in this call site: `load` promotes and then enforces
        // (healing the file on the way), while this reader has no side effects
        // to heal with and so removes the keys outright — see the block comment
        // above `promote_overlay_mcp_registry` for why removal and not
        // `strip_mcp_registry`. No Events row: a lightweight subprocess has no
        // lane to speak into, and the app's own `load` already reported it.
        let _ = strip_overlay_for_readonly_merge(&mut overlay);
        deep_merge(&mut merged, overlay);
    }
    let mut settings: Settings = serde_json::from_value(merged).unwrap_or_default();
    // V40 review M-1: the same parse boundary, and NOT optional here — the
    // children this serves read declared `ext` values (`harness_ext_bool` and
    // friends) to decide what they advertise. No write-back: this reader's whole
    // contract is that it has no side effects.
    settings.normalize_harness_settings();
    settings
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
    let mut s: Settings = fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // **THE parse boundary for the harness map** (V40 Phase B, locked decision
    // 6). Every typed read of a settings file in this module goes through here
    // — the load path, the out-of-band readers, the read-modify-write helpers —
    // so a declared `ext` key whose stored value its kind rejects is replaced by
    // the declared default exactly once, wherever the file was read from.
    //
    // Deliberately NOT in `integrity_check`: that runs only on load-from-disk,
    // and `mutate_global_harness` would then read a hand-edited
    // `"statusline": "yes"`, write it straight back, and hand the launch path a
    // string every reader answers `false` for.
    s.normalize_harness_settings();
    s
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
        return crate::pricing::default_llm_pricing();
    };
    read_llm_pricing_from(&path)
}

fn read_llm_pricing_from(path: &Path) -> Vec<LlmPricingModel> {
    if !path.exists() {
        return crate::pricing::default_llm_pricing();
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .map(|s| s.llm_pricing)
        .unwrap_or_else(crate::pricing::default_llm_pricing)
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
type HarnessMap = std::collections::BTreeMap<String, crate::settings::HarnessSettings>;

static HV_CACHE: std::sync::Mutex<Option<(std::time::SystemTime, HarnessVersions, HarnessMap)>> =
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
        if let Some((cached_at, hv, _)) = cache.as_ref() {
            if *cached_at == mtime {
                return hv.clone();
            }
        }
    }
    let s = read_settings_or_default(&path);
    let hv = s.harness_versions;
    if let Ok(mut cache) = HV_CACHE.lock() {
        *cache = Some((mtime, hv.clone(), s.harness));
    }
    hv
}

/// V40 Phase B: the per-harness settings map, read straight from the physical
/// global file — the same out-of-band discipline (and the same cache) as
/// [`read_global_harness_versions`], and for the same reason: `harness` carries
/// the version the transcript tap observed and the auto-verify record, both
/// written by background threads that must never land in a project overlay
/// diff.
pub fn read_global_harness_map() -> HarnessMap {
    let Ok(path) = global_path() else {
        return crate::settings::default_harness_settings();
    };
    let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else {
        return crate::settings::default_harness_settings();
    };
    if let Ok(cache) = HV_CACHE.lock() {
        if let Some((cached_at, _, map)) = cache.as_ref() {
            if *cached_at == mtime {
                return map.clone();
            }
        }
    }
    let s = read_settings_or_default(&path);
    let map = s.harness.clone();
    if let Ok(mut cache) = HV_CACHE.lock() {
        *cache = Some((mtime, s.harness_versions, map.clone()));
    }
    map
}

/// The reserved AI tab ids the user has enabled, read straight from the
/// physical global file (V40 review finding M-2, parity lens).
///
/// The out-of-band consumer is the auto-verify worker, which runs on a plain
/// thread with no `SettingsHandle` and decides whether to spawn a harness's own
/// CLI to probe it. Deliberately the GLOBAL value rather than a project-merged
/// one: this answers "does this machine use that harness at all", and a
/// background probe is not worth reading a project overlay to decide.
///
/// Uncached — it is asked at most a handful of times per launch, and the
/// `HV_CACHE` above is keyed to the two harness structures.
pub fn read_global_enabled_ai_tabs() -> Vec<crate::settings::AiTabId> {
    let Ok(path) = global_path() else {
        return Vec::new();
    };
    if !path.exists() {
        return Vec::new();
    }
    read_settings_or_default(&path).enabled_ai_tabs
}

/// One harness's row out of [`read_global_harness_map`], defaults included.
///
/// The read every out-of-band consumer wants: `Settings::harness_settings`
/// resolves declared defaults for an absent key, and a raw map lookup would
/// answer `None` where the accessor answers the default.
pub fn read_global_harness_settings(
    harness: crate::harness::HarnessId,
) -> crate::settings::HarnessSettings {
    let probe = crate::settings::Settings {
        harness: read_global_harness_map(),
        ..Default::default()
    };
    probe.harness_settings(harness).clone()
}

/// Mutate ONE harness's row in the physical global file, out of band.
///
/// The `mutate_global_harness_versions` pattern, and it exists for the same
/// two reasons: these fields are stripped from a project overlay by
/// [`strip_overlay_harness`] so a Settings save can never carry them, and the
/// writers here (the version tap, the auto-verify worker, *Mark verified*) run
/// on background threads where a full `save_settings` would race the user's own
/// edits. No-op when the mutation
/// changes nothing, so a change-guarded caller can poll freely.
pub fn mutate_global_harness(
    harness: crate::harness::HarnessId,
    mutate: impl FnOnce(&mut crate::settings::HarnessSettings),
) -> AppResult<crate::settings::HarnessSettings> {
    let Some(id) = harness.id() else {
        return Err(AppError::Settings(
            "harness settings write for an id no registry claims".to_string(),
        ));
    };
    let path = global_path()?;
    let mut settings = read_settings_or_default(&path);
    let row = settings
        .harness
        .entry(id.to_string())
        .or_insert_with(|| crate::settings::HarnessSettings::defaults_for(harness));
    let before = row.clone();
    mutate(row);
    if *row == before {
        return Ok(before);
    }
    let after = row.clone();
    let map = settings.harness.clone();
    let hv = settings.harness_versions.clone();
    save_to(&path, &settings)?;
    if let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) {
        if let Ok(mut cache) = HV_CACHE.lock() {
            *cache = Some((mtime, hv, map));
        }
    }
    Ok(after)
}

// `mutate_global_harness_versions` is gone with the five fields it wrote (V40
// Phase B). Every one of them — the two versions, the verified stamp, the
// auto-verify record, the input-profile spike — is a `harness[<id>]` row now,
// written through [`mutate_global_harness`]. What is LEFT on `HarnessVersions`
// (`e1_status`, `d0_status`) has never had a writer in the app: both are
// recorded by hand after a manual spike, which is what their docs say, so a
// writer nothing called was a function pretending there was a path.

/// Record a harness version observation (V16 Feature 1's tripwire input).
/// `harness` is a registry id — Claude's comes from the OOB transcript tap,
/// OpenCode's from `opencode --version` at tab spawn. Change-guarded — safe to
/// call once per session/spawn without file churn.
///
/// V35 Phase F: a **changed** version is also the first of the two auto-verify
/// triggers (the other is the startup check). It fires from here rather than
/// from the tap because this is the one place the observation is actually
/// recorded — a caller-side trigger would miss the hand-edit and spawn-time
/// paths, and would fire on the no-op re-observations this function exists to
/// swallow. The call is non-blocking (it spawns a detached worker) so the tap
/// is never delayed by a probe.
///
/// **V40 Phase B: one write, whatever the harness.** Phase A had already made
/// the DISPATCH registry-driven; what remained was a two-arm `match id` over
/// `claude_last_seen` / `opencode_last_seen` — a field pair with only one
/// half wired to the auto-verify trigger, so OpenCode's version moving recorded
/// a string and did nothing else. Both halves are `harness[<id>].last_seen`
/// now, and the trigger fires for whichever harness changed.
pub fn note_harness_version(harness: &str, version: &str) {
    let version = version.trim();
    if version.is_empty() {
        return;
    }
    // A version note for a harness nobody registered is dropped, loudly enough
    // to find in a log rather than landing on a `_ => {}` that reads like an
    // intentional no-op.
    let Some(id) = crate::harness::HarnessId::from_id(harness) else {
        tracing::debug!(harness, "version note for an unregistered harness; dropped");
        return;
    };
    let mut changed = false;
    let res = mutate_global_harness(id, |row| {
        changed = row.last_seen != version;
        row.last_seen = version.to_string();
    });
    if let Err(e) = res {
        tracing::warn!("failed to record {harness} version {version}: {e}");
        return;
    }
    if changed {
        crate::harness::verify::on_version_changed(id);
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
///
/// The second half of the answer is the `schema_version` the file **stated
/// before migration** (V40 Phase I, issue #107 item 5). It is what [`load`]
/// falls back to for an overlay written by a build that predates the overlay
/// stamp: an overlay sitting beside a v35 global file was written against a v35
/// baseline, so that is the version to enter its cascade at. `None` for a file
/// that stated none (pre-v1.10, before `schema_version` existed) and for every
/// path that never got to read one.
fn load_global(default_shell: &ShellSpec) -> (Settings, Option<u64>) {
    let path = match global_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "settings: cannot resolve global path; using defaults");
            return (seeded_defaults(default_shell), None);
        }
    };

    if !path.exists() {
        let s = seeded_defaults(default_shell);
        if let Err(e) = save_to(&path, &s) {
            tracing::warn!(error = %e, path = %path.display(), "settings: write global defaults failed");
        } else {
            tracing::info!(path = %path.display(), "settings: wrote global defaults");
        }
        return (s, None);
    }

    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read global failed; using defaults");
            return (seeded_defaults(default_shell), None);
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
            return (s, None);
        }
    };

    // Read the stated version BEFORE migrating — after it, every file says
    // `CURRENT`, and the version an overlay beside this file was written
    // against is gone (see this function's doc comment).
    let stated = migration::stated_schema_version(&value);

    // **The migration floor** (V42 R9, issue #120). Strictly after the
    // fresh-install branch above — a file that is MISSING is seeded, never
    // quarantined — and strictly before the cascade, which has no steps left for
    // a file this old and would otherwise rewrite it as current with its
    // contents silently defaulted.
    if let Some(reseeded) = reseed_below_floor(&path, &value, default_shell) {
        return (reseeded, None);
    }

    // Migrate the global file in place. Backup is named after the global
    // file, which is the source of truth for the global baseline shape.
    let migrated = match migration::migrate_if_needed(&mut value, &path) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                path = %path.display(),
                "settings: global migration aborted (backup failed); using defaults"
            );
            return (seeded_defaults(default_shell), None);
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
            return (s, None);
        }
    };

    // V14 Phase A: seed the starter prompt-template library exactly once,
    // directly against the physical global file — this is the ONE place
    // that runs (unlike `integrity_check`, which also runs against the
    // per-project merged `Settings` and wouldn't reliably flush a fresh
    // seed to disk when a project overlay is active; see the function's
    // own doc comment).
    let seeded = seed_prompt_templates_if_needed(&mut typed);
    // F-19: same reasoning — the physical global file is the only place the
    // price table lives, so the top-up has to run here rather than against the
    // merged per-project `Settings`.
    let priced = top_up_llm_pricing_if_needed(&mut typed);
    // **THE parse boundary for the harness map, on the LOAD path** (V40 review
    // finding M-1). `read_settings_or_default`'s doc claimed every typed read in
    // this module went through it; this one did not, so a hand-edited
    // `"statusline": "yes"` reached the launch path as a string the accessors
    // answer with the DECLARED DEFAULT (`true`) while the Settings window's
    // `value === true` rendered it OFF — the UI saying one thing and the spawn
    // doing the other, which is precisely the divergence `SettingKind::accepts`
    // exists to prevent. Folded into the write-back below so the repair is
    // durable rather than re-derived (and re-warned) on every launch.
    let normalized = typed.normalize_harness_settings();

    if migrated || seeded || priced || normalized {
        // Persist the migrated/seeded shape back to disk so future launches
        // don't re-migrate or re-seed. Atomic write inside save_to keeps
        // this safe under crash.
        if let Err(e) = save_to(&path, &typed) {
            tracing::warn!(error = %e, path = %path.display(), "settings: post-migration/seed global save failed");
        } else {
            tracing::info!(path = %path.display(), migrated, seeded, normalized, "settings: global migrated/seeded and rewritten");
        }
    } else {
        tracing::info!(path = %path.display(), "settings: global loaded");
    }
    (typed, stated)
}

/// **The global migration floor** (V42 R9, issue #120).
///
/// `value` is the global file at `path`, already read and parsed as JSON. If it
/// states a schema below [`migration::MIN_GLOBAL_SCHEMA_VERSION`] — or states
/// none at all, which is what a pre-v1.10 file looks like — it is moved aside
/// INTACT, fresh defaults are written in its place, and those defaults are
/// returned. `None` means the file is at or above the floor and the caller
/// carries on into the cascade.
///
/// **Why not just parse it.** Because that succeeds. `Settings` carries a
/// container-level `#[serde(default)]`, so an old file deserializes cleanly with
/// every field the deleted v1.0 → v29 steps would have MOVED quietly reset to a
/// default — and is then written back still stamped at its old version, so no
/// later launch ever notices. Loud beats silent, and a file the user still has
/// beats a file they do not.
///
/// **What is shared with the corrupt path, and what is not.** The mechanism is
/// shared on purpose ([`migration::quarantine_outdated_file`] is
/// [`migration::quarantine_corrupt_file`]'s twin over one helper): the outcome
/// the user needs is the same — the app launches, their old file is still on
/// disk. The WORDING is deliberately not shared. This file is valid JSON written
/// by an older cImp, not a broken one, and calling a user's intact settings
/// "corrupt" sends them looking for the wrong problem. The quarantine file says
/// so too: `.outdated.` rather than `.corrupted.`.
///
/// **If the move fails, nothing is overwritten.** The corrupt path reseeds
/// regardless (its bytes are unreadable anyway); here the bytes are the user's
/// readable settings, so a failed move means we hand back defaults for this
/// session and leave the file exactly where it is. It stays loud on every launch
/// rather than becoming quiet and gone once.
fn reseed_below_floor(path: &Path, value: &Value, default_shell: &ShellSpec) -> Option<Settings> {
    if !migration::below_global_floor(value) {
        return None;
    }
    let stated = migration::stated_schema_version(value);
    let Some(quarantine) = migration::quarantine_outdated_file(path) else {
        tracing::error!(
            ?stated,
            floor = migration::MIN_GLOBAL_SCHEMA_VERSION,
            path = %path.display(),
            "settings: the global settings file was written by a version of cImp too old for \
             this build to upgrade, AND it could not be moved aside — running on defaults for \
             this session and leaving the file untouched rather than overwriting it"
        );
        return Some(seeded_defaults(default_shell));
    };
    tracing::error!(
        ?stated,
        floor = migration::MIN_GLOBAL_SCHEMA_VERSION,
        path = %path.display(),
        quarantine = %quarantine.display(),
        "settings: the global settings file was written by a version of cImp too old for this \
         build to upgrade (its schema is below the migration floor). It has NOT been read and \
         NOT been deleted: it was moved aside intact to the quarantine path below, and fresh \
         defaults were written in its place"
    );
    let s = seeded_defaults(default_shell);
    if let Err(e) = save_to(path, &s) {
        tracing::warn!(error = %e, path = %path.display(), "settings: write global defaults after quarantine failed");
    }
    Some(s)
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

// ── The machine-scope matrix ────────────────────────────────────────────────
//
// Several settings families are MACHINE scope in whole or in part: they
// describe this install (or this machine's OS boundary), not this checkout, and
// a project overlay must never carry them. Each one has to answer the SAME set
// of questions, on three different legs:
//
//   * [`load`]           — strip the family out of the overlay before the
//                          merge, and/or promote a legacy overlay's copy into
//                          the global baseline and then ENFORCE the baseline
//                          over the merged view.
//   * [`load_readonly`]  — the same strip for the `cimp --offload-mcp` child,
//                          which has no side effects to heal with.
//   * [`save`]           — write the live value THROUGH to the physical global
//                          file (the only place a Settings-window edit of it
//                          can land, since the diff strips it), then normalize
//                          both diff sides so the overlay carries no copy.
//
// Until V42 those cells were sixteen hand-written functions wired by
// hand-enumerated call sites, and the failure mode of a missed cell is not a
// crash: it is machine state silently leaking into a portable overlay, or a
// setting the Settings window can edit and then cannot save. That bug was found
// and fixed twice — for `tool_plugins` in V38 and for `harness` in the V40
// review (finding M-2) — which is two more times than a shape that made it
// impossible would have needed.
//
// The sixteen functions are unchanged. What changed is that they are now CELLS
// OF A TABLE the three legs iterate, so adding a family is adding one row plus
// its functions: there is no call site to remember, every optional cell must be
// either filled or given a written reason for being empty (`readonly_exempt`,
// `sync_writer`), and `every_top_level_setting_declares_its_scope` fails until
// a newly added top-level settings key says which side of the line it is on.
// (`machine_scope_phase_order_is_pinned` also wants the new name, deliberately:
// the order the legs run in is a record someone confirms, not a side effect.)
//
// ORDER IS PART OF THE TABLE, and two orderings are load-bearing:
//
//   * the wholesale ban ([`strip_overlay_banned`]) runs BEFORE the structured
//     strips, on every leg — the walks below do it, not their callers;
//   * `enforce` runs AFTER `deep_merge`, never before.
//
// `machine_scope_phase_order_is_pinned` freezes the per-leg row order. The
// three strip legs run in exactly the order the hand-written call sites ran in.
// The `promote` and `sync` legs are in table order rather than their pre-V42
// order, which is safe because no two rows own overlapping keys
// (`machine_scope_families_own_disjoint_keys`) — every promoter and every
// syncer still runs (the hand-written sites computed all of them into locals
// and then OR'd, which the walks' `|=` preserves; a `||` over the CALLS would
// short-circuit and skip a later family's one-time heal), and each writes into
// its own field of the same value.

/// How one leg removes a machine-scope family from a settings value.
enum OverlayStrip {
    /// Nothing to remove on this leg.
    ///
    /// Legal only where the row says why: `promote` + `enforce` cover the
    /// family instead (a global-authority family's LOAD leg), or
    /// `readonly_exempt` names the reason a read-only reader needs none. See
    /// `every_machine_scoped_family_fills_or_explains_every_cell`.
    Nothing,
    /// Covered by the wholesale [`strip_overlay_banned`] pass that runs at the
    /// head of every strip leg — this row's keys are in [`OVERLAY_BANNED_KEYS`].
    ///
    /// A marker rather than a hook, because the ban is ONE pass over the value
    /// for all of them and it has to run before the structured strips.
    /// `the_banned_rows_and_overlay_banned_keys_agree` keeps the marker and the
    /// list from drifting apart.
    Banned,
    /// A structured strip: only part of the subtree is machine scope, so it
    /// returns the dotted names of what it dropped for the `plugin` Events lane.
    Named(fn(&mut Value) -> Vec<String>),
    /// A whole-value normalizer with nothing to name.
    ///
    /// Only ever a DIFF-side cell, and that is enforced. These write `[]` into
    /// the family's keys rather than removing them, and `deep_merge` replaces
    /// arrays wholesale — so the same function on an overlay leg would ERASE the
    /// global value instead of ignoring the overlay's, which is exactly what
    /// `the_save_side_normalizer_would_erase_the_global_registry_on_the_load_side`
    /// pins.
    Normalize(fn(&mut Value)),
}

impl OverlayStrip {
    /// Apply this cell to `v`, returning the dotted names worth reporting.
    fn apply(&self, v: &mut Value) -> Vec<String> {
        match self {
            // `Banned` is applied wholesale at the head of the leg, before any
            // structured strip runs.
            Self::Nothing | Self::Banned => Vec::new(),
            Self::Named(f) => f(v),
            Self::Normalize(f) => {
                f(v);
                Vec::new()
            }
        }
    }
}

/// One machine-scope settings family — a cell for every leg that has to know
/// about it. See the block comment above.
///
/// Four cells (`name`, `keys`, `readonly_exempt`, `sync_writer`) are
/// DECLARATIONS the coverage tests read rather than hooks the legs call: they
/// are why an empty cell is a test failure instead of a silent hole. Each
/// carries its own `allow(dead_code)` — per field on purpose, so a HOOK that
/// stops being called is still reported.
struct MachineScopedField {
    /// Stable family name: diagnostics, and the coverage tests' identity.
    #[allow(dead_code)]
    name: &'static str,
    /// The dotted wire paths this family owns — a top-level key for a
    /// whole-key family, `container.field` for one that shares a container with
    /// project-scoped siblings. No two rows may overlap
    /// (`machine_scope_families_own_disjoint_keys`), which is what makes the
    /// `promote` and `sync` legs order-independent.
    #[allow(dead_code)]
    keys: &'static [&'static str],
    /// [`load`], on the overlay value before the merge.
    overlay_strip: OverlayStrip,
    /// [`load_readonly`], on the overlay value before the merge.
    readonly_strip: OverlayStrip,
    /// Why `readonly_strip` is [`OverlayStrip::Nothing`] — required exactly when
    /// it is, so no family can be exempted by omission. The MCP registry's
    /// exemption was exactly that omission until the V38 merge review found it.
    #[allow(dead_code)]
    readonly_exempt: Option<&'static str>,
    /// [`load`]: fold a legacy overlay's copy into the global baseline. Returns
    /// "the caller should persist".
    promote: Option<fn(&mut Settings, &Value) -> bool>,
    /// [`load`], **after `deep_merge`**: overwrite the merged view from the
    /// (post-promotion) global baseline.
    enforce: Option<fn(&mut Value, &Settings)>,
    /// [`save`]: write the live value through to the physical global file.
    /// Returns "the file is worth rewriting".
    sync: Option<fn(&mut Settings, &Settings) -> bool>,
    /// The dedicated out-of-band writer that stands in for `sync` — required
    /// exactly when `sync` is `None`, because a machine-scope family with
    /// neither is one the Settings window can edit and then never save.
    #[allow(dead_code)]
    sync_writer: Option<&'static str>,
    /// [`save`]: normalize both diff sides so no overlay pins a copy.
    diff_strip: OverlayStrip,
}

/// Every machine-scope settings family, in the order the legs walk them.
const MACHINE_SCOPED: &[MachineScopedField] = &[
    // The three whole-key bans first — see [`OVERLAY_BANNED_KEYS`] for why each
    // is per-install state, and `sync_writer` for where an edit of it lands.
    MachineScopedField {
        name: "llm_pricing",
        keys: &["llm_pricing"],
        overlay_strip: OverlayStrip::Banned,
        readonly_strip: OverlayStrip::Banned,
        readonly_exempt: None,
        promote: None,
        enforce: None,
        sync: None,
        sync_writer: Some("write_global_llm_pricing"),
        diff_strip: OverlayStrip::Banned,
    },
    MachineScopedField {
        name: "harness_versions",
        keys: &["harness_versions"],
        overlay_strip: OverlayStrip::Banned,
        readonly_strip: OverlayStrip::Banned,
        readonly_exempt: None,
        promote: None,
        enforce: None,
        sync: None,
        sync_writer: Some("mutate_global_harness"),
        diff_strip: OverlayStrip::Banned,
    },
    // V33: banned because the overlay file lives INSIDE the boundary this block
    // configures. The write-through is what keeps it savable at all, which is
    // the thing a plain ban would have broken.
    MachineScopedField {
        name: "sandbox",
        keys: &["sandbox"],
        overlay_strip: OverlayStrip::Banned,
        readonly_strip: OverlayStrip::Banned,
        readonly_exempt: None,
        promote: None,
        enforce: None,
        sync: Some(sync_sandbox_into),
        sync_writer: None,
        diff_strip: OverlayStrip::Banned,
    },
    // Global libraries that ride two `offload` array fields: promoted once by
    // name, then enforced over the merged view, so an overlay carries no
    // authority and needs no strip on the LOAD leg.
    MachineScopedField {
        name: "offload_templates",
        keys: &[
            "offload.server_command_templates",
            "offload.remote_backend_templates",
        ],
        overlay_strip: OverlayStrip::Nothing,
        readonly_strip: OverlayStrip::Nothing,
        readonly_exempt: Some(
            "the MCP children never read the template libraries — they are a \
             Settings-UI paste convenience with no consumer behind `load_readonly`",
        ),
        promote: Some(promote_overlay_offload_templates),
        enforce: Some(enforce_global_offload_templates),
        sync: Some(sync_offload_templates_into),
        sync_writer: None,
        diff_strip: OverlayStrip::Normalize(strip_offload_templates),
    },
    // V38: the first block whose two scopes interleave rather than separate by
    // key, so the strip is structured and is an ALLOW-list. `promote` here is
    // the one-time legacy `code_audit.tools` fold — the SOURCE key is not this
    // family's (it is dead schema), the destination is.
    MachineScopedField {
        name: "tool_plugins",
        keys: &["tool_plugins"],
        overlay_strip: OverlayStrip::Named(strip_overlay_tool_plugins),
        readonly_strip: OverlayStrip::Named(strip_overlay_tool_plugins),
        readonly_exempt: None,
        promote: Some(promote_overlay_audit_config),
        enforce: None,
        sync: Some(sync_tool_plugin_state_into),
        sync_writer: None,
        diff_strip: OverlayStrip::Named(strip_overlay_tool_plugins),
    },
    // V40 review M-2: the same structured strip for the per-harness map, whose
    // scope is per FIELD — and a DENY-list, deliberately the opposite of
    // `tool_plugins` (see [`OVERLAY_BANNED_HARNESS_FIELDS`]).
    MachineScopedField {
        name: "harness",
        keys: &["harness"],
        overlay_strip: OverlayStrip::Named(strip_overlay_harness),
        readonly_strip: OverlayStrip::Named(strip_overlay_harness),
        readonly_exempt: None,
        promote: None,
        enforce: None,
        sync: Some(sync_harness_into),
        sync_writer: None,
        diff_strip: OverlayStrip::Named(strip_overlay_harness),
    },
    // V37 F5: the registry is global, activation is per-project. The one family
    // whose two overlay legs differ, and the difference lives HERE rather than
    // in either reader: `load` promotes and then enforces (healing the file on
    // the way), `load_readonly` has no side effects to heal with and so removes
    // the keys — with the removal function, never the `[]`-writing normalizer.
    MachineScopedField {
        name: "mcp_registry",
        keys: &["offload.mcp_servers", "offload.mcp_categories"],
        overlay_strip: OverlayStrip::Nothing,
        readonly_strip: OverlayStrip::Named(strip_overlay_mcp_registry),
        readonly_exempt: None,
        promote: Some(promote_overlay_mcp_registry),
        enforce: Some(enforce_global_mcp_registry),
        sync: Some(sync_mcp_registry_into),
        sync_writer: None,
        diff_strip: OverlayStrip::Normalize(strip_mcp_registry),
    },
];

/// [`load`]'s overlay leg: strip every machine-scope family from an overlay
/// value before the merge, returning the dotted names the structured strips
/// dropped (the whole-key bans name nothing — see [`OverlayStrip::Banned`]).
///
/// The wholesale ban runs first, then the rows in table order.
fn strip_overlay_for_merge(v: &mut Value) -> Vec<String> {
    strip_overlay_banned(v);
    let mut dropped = Vec::new();
    for row in MACHINE_SCOPED {
        dropped.extend(row.overlay_strip.apply(v));
    }
    dropped
}

/// [`load_readonly`]'s overlay leg: the same table, this leg's cells. The
/// caller discards the names — a lightweight subprocess has no Events lane, and
/// the app's own [`load`] has already reported them.
fn strip_overlay_for_readonly_merge(v: &mut Value) -> Vec<String> {
    strip_overlay_banned(v);
    let mut dropped = Vec::new();
    for row in MACHINE_SCOPED {
        dropped.extend(row.readonly_strip.apply(v));
    }
    dropped
}

/// [`load`]'s promote leg: fold a legacy overlay's copies into the global
/// baseline. True ⇒ the caller must persist.
///
/// **Every promoter runs**, which is why the accumulation is `|=` and not a
/// `||` chain over the calls. The pre-V42 site computed all three into locals
/// and then OR'd them, so it ran them all too; a short-circuit here would skip
/// a later family's one-time heal — and `promote_overlay_mcp_registry` returns
/// true for "promoted nothing, but the overlay still carries a registry key",
/// which is a rewrite request, not a no-op.
fn promote_overlay_into_global(global: &mut Settings, overlay: &Value) -> bool {
    let mut changed = false;
    for row in MACHINE_SCOPED {
        if let Some(promote) = row.promote {
            changed |= promote(global, overlay);
        }
    }
    changed
}

/// [`load`]'s enforce leg — **after `deep_merge`, never before**: overwrite the
/// merged view's global-authority fields from the (post-promotion) baseline.
fn enforce_global_machine_scope(merged: &mut Value, global: &Settings) {
    for row in MACHINE_SCOPED {
        if let Some(enforce) = row.enforce {
            enforce(merged, global);
        }
    }
}

/// [`save`]'s write-through leg: copy every family's live value onto the
/// on-disk global settings. True ⇒ the physical file is worth rewriting.
///
/// Every syncer runs, for the same reason every promoter does.
fn sync_machine_scope_into(disk_global: &mut Settings, current: &Settings) -> bool {
    let mut changed = false;
    for row in MACHINE_SCOPED {
        if let Some(sync) = row.sync {
            changed |= sync(disk_global, current);
        }
    }
    changed
}

/// [`save`]'s diff leg: normalize BOTH sides identically, so what is left is
/// only what a project may legitimately carry and the diff can express the
/// project's overrides and nothing else.
///
/// The structured strips' return values are discarded here: a strip of OUR OWN
/// serialized value is not a user's hand edit, so there is nothing to warn
/// about — the load path is where a warning belongs.
fn strip_machine_scope_from_diff(current: &mut Value, baseline: &mut Value) {
    strip_overlay_banned(current);
    strip_overlay_banned(baseline);
    for row in MACHINE_SCOPED {
        let _ = row.diff_strip.apply(current);
        let _ = row.diff_strip.apply(baseline);
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
///
/// # `sandbox` is banned for a different reason: it is a SECURITY boundary
///
/// V33 (2026-08-18). The other two rows are here to stop stale state shadowing
/// a global write. `sandbox` is here because **the overlay file lives inside
/// the boundary the block configures**: `.cimp/config.json` sits under the
/// project root, the project root is granted FULL access to every sandboxed
/// child, and [`load_readonly`] deep-merges that overlay on every MCP-child
/// call (`graph::mcp::current_settings`). Without this ban a confined child
/// could write its own `.cimp/config.json` and:
///
/// * set `sandbox.enabled = false` — the boundary is simply gone on the next
///   spawn; or
/// * add `sandbox.extra_grant_dirs: ["C:\\Users\\<u>\\.ssh"]` — and cImp,
///   running as the user, would stamp a DURABLE inheritable ACE granting the
///   container read access to the user's credentials.
///
/// A boundary a confined process can widen is not a boundary. `sandbox` is
/// therefore **machine/global scope**: it describes what this machine's OS will
/// enforce, not what one checkout prefers, and it is edited in the global
/// settings file only. [`save`] writes the live block THROUGH to the physical
/// global file (`sync_sandbox_into`) so the Settings window still works from
/// inside a customized project, and [`load`] deliberately does **not** promote
/// an existing overlay's `sandbox` block into the global baseline the way the
/// audit scanner paths are promoted — promotion would honor exactly the
/// contaminated file this ban exists to ignore. A user who had configured
/// sandboxing per-project before this landed re-sets it once, globally.
///
/// This is one of TWO independent mitigations; the other is
/// [`crate::sandbox::extra_grant_refusal`], which refuses a credential or
/// system directory at the grant site whatever settings file named it. Neither
/// is sufficient alone: banning the key still leaves a compromised *global*
/// file able to name `~/.ssh`, and screening paths still leaves
/// `sandbox.enabled` flippable.
// V40 Phase B added `harness` to this list and the review (finding M-2) took it
// back out. `harness` **cannot** be banned wholesale, for the same reason
// `tool_plugins` cannot: its scope is per FIELD, not per container. Five of the
// settings that moved into it were per-project on develop
// (`statusline.enabled`, `claude_local.*`, `code_audit.expose_<id>`,
// `offload.opencode_provider{,_auto}`,
// `offload.injection.opencode_native_gate_enabled`), and banning the container
// silently narrowed all five to machine scope — a scope change hiding inside a
// refactor, with the first post-upgrade save erasing the project's values and
// no trace anywhere. The machine-scope half gets [`strip_overlay_harness`]
// instead, which names what it drops.
const OVERLAY_BANNED_KEYS: &[&str] = &["llm_pricing", "harness_versions", "sandbox"];

fn strip_overlay_banned(v: &mut Value) {
    if let Value::Object(map) = v {
        for k in OVERLAY_BANNED_KEYS {
            map.remove(*k);
        }
    }
}

/// SAVE write-through for the machine-scope `sandbox` block: copy the live
/// value onto the on-disk global settings, returning true when it changed.
///
/// `sandbox` is in [`OVERLAY_BANNED_KEYS`], so [`save`]'s diff can never carry
/// it into a project overlay — which would leave a Settings-window edit with
/// nowhere to land if this did not exist. Same pattern as
/// [`sync_tool_plugin_state_into`]: the pure half, so the caller decides
/// whether the physical file is worth rewriting. (It used to name
/// `sync_audit_paths_into`, which V38 Phase E deleted along with the per-tool
/// array it split.)
///
/// Whole-block, not per-field: every field of `SandboxSettings` is the same
/// scope (what the OS enforces on this machine), and a per-field copy would be
/// a place for a newly added field to be silently forgotten.
fn sync_sandbox_into(disk_global: &mut Settings, current: &Settings) -> bool {
    if disk_global.sandbox == current.sandbox {
        return false;
    }
    disk_global.sandbox = current.sandbox.clone();
    true
}

// ── Legacy audit-tool config: one-time promotion into `tool_plugins` ─────────
//
// Before schema v33, the fourteen built-in scanners were configured through
// `code_audit.tools`, a project-scoped array with one machine-scoped field
// inside it (`path`), split apart here by a promote-on-load / write-through-on-
// save pair. V38 moved the whole of that configuration into the `tool_plugins`
// container, where the scope split is structural rather than enforced by these
// functions — so the pair is gone and only the LEGACY DATA question is left.
//
// That question is real and easy to miss: the v32 → v33 migration rewrites the
// GLOBAL settings file, and **a project overlay is never schema-migrated** (see
// `load`). A user who configured audit tools from inside a project has that
// configuration in `.cimp/config.json` as a `code_audit.tools` array, which the
// new schema simply does not have a field for — so without this it would be
// silently dropped the first time they launched the new build. "The setting
// moved, so I threw yours away" is a data-loss bug wearing an upgrade's costume.
//
// So: promote what the overlay carries into the container's EMPTY slots, once,
// and let the next save write the overlay out in its new shape (the diff is
// recomputed whole, so the stale key disappears by itself). Idempotent — it only
// ever fills a slot the container does not already have — which matters because
// a project that is never saved would otherwise re-promote on every launch.

/// The v33 container key each legacy `code_audit.tools[].id` maps to.
///
/// A plain string join rather than a lookup: the ids were `AuditToolId`'s wire
/// names and the built-in manifest reuses them verbatim, which is the property
/// that makes the migration a rename of the STORAGE and not of the tools.
fn legacy_tool_key(id: &str) -> String {
    format!("{}/{id}", crate::plugins::builtin::AUDIT_PLUGIN_KEY)
}

/// Is `id` a tool this build actually ships under the built-in audit plugin?
///
/// **Phase E gate, B-E2.** The legacy overlay is a JSON file a user (or an
/// older build, or a hand edit) wrote, and nothing has ever validated the ids
/// in it — the pre-v34 reader dropped an unknown one at deserialize time, and
/// the v33 → v34 migration drops it explicitly. This is the third reader of
/// the same array and it has to make the same call, because the two things it
/// writes are worse than a dropped setting: a container slot for a tool that
/// does not exist is husk state the settings pane cannot show or clear, and a
/// `global_paths` entry is a MACHINE-WIDE path keyed on a name nothing will
/// ever resolve — a fabricated id in one project's overlay minting machine
/// state is a wider blast radius than the promotion is worth.
///
/// Reads the embedded set rather than a frozen list: unlike a migration step
/// (which describes a file shape on a fixed date — ruling R4), this describes
/// what THIS build can run, so it should move when the roster moves.
fn legacy_id_is_shipped(id: &str) -> bool {
    crate::plugins::builtin::plugin_set()
        .plugins
        .iter()
        .filter(|p| p.key == crate::plugins::builtin::AUDIT_PLUGIN_KEY)
        .any(|p| p.manifest.tools.iter().any(|t| t.id == id))
}

/// Promote a legacy overlay's `code_audit.tools` into the global baseline's
/// `tool_plugins` container, filling only slots the container does not have.
/// Returns true when `global` changed — the caller persists via the post-load
/// `save`, which also rewrites the overlay in its new shape.
fn promote_overlay_audit_config(global: &mut Settings, overlay: &Value) -> bool {
    let Some(entries) = overlay
        .get("code_audit")
        .and_then(|c| c.get("tools"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let mut changed = false;
    for e in entries {
        let Some(id) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        // B-E2: an id this build does not ship buys nothing and costs husk
        // state plus a machine-wide path — see `legacy_id_is_shipped`.
        if !legacy_id_is_shipped(id) {
            continue;
        }
        let tool_key = legacy_tool_key(id);

        // The path is machine scope and always was: promote it into the
        // machine-wide map, first project launched wins per slot.
        if let Some(path) = e.get("path").and_then(Value::as_str) {
            let path = path.trim();
            if !path.is_empty() && !global.tool_plugins.global_paths.contains_key(&tool_key) {
                global
                    .tool_plugins
                    .global_paths
                    .insert(tool_key.clone(), path.to_string());
                changed = true;
            }
        }

        // Everything else lands as this tool's state — but only if the
        // container has no state for it yet, so a value the user has since set
        // through the new pane is never overwritten by a stale overlay.
        let plugin = global
            .tool_plugins
            .plugins
            .entry(crate::plugins::builtin::AUDIT_PLUGIN_KEY.to_string())
            .or_default();
        if plugin.tools.contains_key(id) {
            continue;
        }
        let mut state = crate::settings::ToolState::default();
        let mut carried = false;
        if let Some(v) = e.get("enabled").and_then(Value::as_bool) {
            state.enabled = v;
            carried = true;
        }
        if let Some(v) = e.get("timeout_secs").and_then(Value::as_u64) {
            state.timeout_secs = Some(v);
            carried = true;
        }
        if let Some(a) = e.get("extra_args").and_then(Value::as_array) {
            let args: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            if !args.is_empty() {
                state.parameters = args;
                carried = true;
            }
        }
        // An EMPTY legacy ruleset meant "use the tool's built-in default",
        // which in the container is the absence of a value rather than a
        // stored blank — see `registry::effective_tools`.
        if let Some(r) = e.get("ruleset").and_then(Value::as_str) {
            if !r.trim().is_empty() {
                state.variables.insert("ruleset".to_string(), r.to_string());
                carried = true;
            }
        }
        if carried {
            plugin.tools.insert(id.to_string(), state);
            changed = true;
        }
    }
    if changed {
        tracing::info!("settings: promoted a legacy project overlay's audit tool config");
    }
    changed
}

// ── Offload backend template libraries: machine-scope splitting ──────────────
//
// `offload.server_command_templates` and `offload.remote_backend_templates`
// are documented as GLOBAL libraries (a saved llama-server command should be
// loadable from any project), but they rode the normal diff/save flow: a
// template saved while a project overlay was active got pinned in THAT
// project's overlay and every other project saw an empty library. Same
// treatment as the audit scanner paths above, applied to whole arrays:
//
//   * LOAD — a legacy overlay's templates are promoted into the global
//     baseline once (by name; an existing global name is never overwritten),
//     then the merged view's arrays are ALWAYS overwritten from the global
//     baseline: overlays carry no template authority.
//   * SAVE — the live arrays are written through to the PHYSICAL global file
//     and replaced with `[]` on both diff sides so no new overlay carries a
//     copy.
//
// `load_readonly` stays exempt like the audit paths: the MCP children never
// read the template libraries (they're a Settings-UI paste convenience).

/// LOAD step 1: promote a legacy overlay's templates into the global
/// baseline, keyed by name (a name already present globally wins). Returns
/// true when `global` changed — persisted by the caller's post-load `save`,
/// which also rewrites the overlay stripped so promotion is one-time.
fn promote_overlay_offload_templates(global: &mut Settings, overlay: &Value) -> bool {
    let mut changed = false;
    if let Some(entries) = overlay
        .get("offload")
        .and_then(|o| o.get("server_command_templates"))
        .and_then(Value::as_array)
    {
        for e in entries {
            let Ok(t) = serde_json::from_value::<ServerCommandTemplate>(e.clone()) else {
                continue;
            };
            if !t.name.trim().is_empty()
                && !global
                    .offload
                    .server_command_templates
                    .iter()
                    .any(|g| g.name == t.name)
            {
                global.offload.server_command_templates.push(t);
                changed = true;
            }
        }
    }
    if let Some(entries) = overlay
        .get("offload")
        .and_then(|o| o.get("remote_backend_templates"))
        .and_then(Value::as_array)
    {
        for e in entries {
            let Ok(t) = serde_json::from_value::<RemoteBackendTemplate>(e.clone()) else {
                continue;
            };
            if !t.name.trim().is_empty()
                && !global
                    .offload
                    .remote_backend_templates
                    .iter()
                    .any(|g| g.name == t.name)
            {
                global.offload.remote_backend_templates.push(t);
                changed = true;
            }
        }
    }
    changed
}

/// LOAD step 2: overwrite the merged view's template arrays from the
/// (post-promotion) global baseline.
fn enforce_global_offload_templates(merged: &mut Value, global: &Settings) {
    let Some(off) = merged.get_mut("offload").and_then(Value::as_object_mut) else {
        return;
    };
    if let Ok(v) = serde_json::to_value(&global.offload.server_command_templates) {
        off.insert("server_command_templates".to_string(), v);
    }
    if let Ok(v) = serde_json::to_value(&global.offload.remote_backend_templates) {
        off.insert("remote_backend_templates".to_string(), v);
    }
}

/// SAVE step 1 (pure half of the write-through): copy the live template
/// arrays onto the on-disk global settings. Returns true when anything
/// changed — the caller only rewrites the physical file then.
fn sync_offload_templates_into(disk_global: &mut Settings, current: &Settings) -> bool {
    let mut changed = false;
    if disk_global.offload.server_command_templates != current.offload.server_command_templates {
        disk_global.offload.server_command_templates =
            current.offload.server_command_templates.clone();
        changed = true;
    }
    if disk_global.offload.remote_backend_templates != current.offload.remote_backend_templates {
        disk_global.offload.remote_backend_templates =
            current.offload.remote_backend_templates.clone();
        changed = true;
    }
    changed
}

// ── V38 tool plugins: scope splitting INSIDE one subtree ────────────────────
//
// `tool_plugins` is the first settings block whose two scopes are interleaved
// rather than separable by key. Within it:
//
//   * `plugins.*.tools.*.variables` and `.parameters` are PROJECT scope. They
//     are what a repo legitimately differs on — this project's ruleset, this
//     project's extra `--exclude` — and they are the only fields a
//     `.cimp/config.json` may carry.
//   * everything else — `enabled` at either level, `timeout_secs`, and the two
//     path maps — is MACHINE scope.
//
// The paths are machine scope for the reason the V26 field report taught (a
// scanner configured in one repo, an audit run in another resolving nothing)
// AND for the reason V33's `sandbox` ban taught, which is sharper: the overlay
// file lives under the project root, the project root is granted full access to
// every sandboxed child, and a confined tool that could write its own
// `.cimp/config.json` could then point cImp at a different binary — or flip its
// own `enabled` — on the next run. A boundary a confined process can widen is
// not a boundary. `sandbox` could be banned wholesale ([`OVERLAY_BANNED_KEYS`]);
// this block cannot, because two of its leaves genuinely belong to the project.
// So the strip is STRUCTURED instead of a key removal, and it is an ALLOW-list:
// anything not named survives nowhere, including keys a future version adds.
//
// Same two-sided arrangement as the audit paths:
//   * LOAD  — strip the overlay before the merge, and say so once in the
//             `plugin` Events lane when it actually carried something.
//   * SAVE  — write the machine-scope halves through to the PHYSICAL global
//             file, then strip both diff sides so no overlay pins a copy.
//
// `load_readonly` strips too, and that is NOT optional: the MCP children it
// serves are exactly the Phase C/D consumers that will resolve a tool's binary
// path, so an unstripped read there would reintroduce the hole at the one call
// site that runs inside the boundary.

/// The only leaves of `tool_plugins` a project overlay may carry.
const OVERLAY_TOOL_PLUGIN_LEAVES: &[&str] = &["variables", "parameters"];

/// Remove every machine-scope field under `tool_plugins`, returning the dotted
/// names of what was dropped (empty ⇒ the overlay was already clean).
///
/// An allow-list walk, not a deny-list: an unrecognized key is dropped and
/// named. A deny-list would let the next field added to the container ride the
/// overlay by default, which is the wrong direction for a block whose default
/// answer has to be "machine".
///
/// **Shape is part of the allow-list** (Phase B review, B-1). Every level of
/// this subtree except the two leaves is a MAP, and a node that is not one is
/// removed and reported rather than walked past. Walking past it was a hole
/// with teeth: `"plugins": {"acme@1.0.0": 5}` survived the strip, `deep_merge`
/// then scalar-overwrote the user's stored object with `5`, the lenient reader
/// dropped the unreadable node, and the registry's "no state ⇒ the user has
/// never touched this ⇒ enabled" default re-enabled a tool the user had
/// switched OFF — from a file every sandboxed child can write. `null` is the
/// same case: the merge deletes the subtree under it.
///
/// The one place a `null` is legitimate is a `variables`/`parameters` LEAF,
/// where it is how [`diff`] spells "this project clears the machine's value" —
/// so those two are handed through untouched, whatever they hold.
fn strip_overlay_tool_plugins(v: &mut Value) -> Vec<String> {
    let mut dropped: Vec<String> = Vec::new();
    let Some(root) = v.as_object_mut() else {
        return dropped;
    };
    let Some(tp) = root.get_mut("tool_plugins") else {
        return dropped;
    };
    let Some(tp_obj) = tp.as_object_mut() else {
        // Not an object at all: there is nothing here we could keep.
        root.remove("tool_plugins");
        dropped.push("tool_plugins".to_string());
        return dropped;
    };

    for key in keys_other_than(tp_obj, &["plugins"]) {
        tp_obj.remove(&key);
        dropped.push(format!("tool_plugins.{key}"));
    }
    if remove_if_not_a_map(tp_obj, "plugins") {
        dropped.push("tool_plugins.plugins".to_string());
    }
    if let Some(plugins) = tp_obj.get_mut("plugins").and_then(Value::as_object_mut) {
        for key in non_map_keys(plugins) {
            plugins.remove(&key);
            dropped.push(format!("tool_plugins.plugins.{key}"));
        }
        for (plugin_key, state) in plugins.iter_mut() {
            // Every survivor of `non_map_keys` is an object.
            let Some(p) = state.as_object_mut() else {
                continue;
            };
            for key in keys_other_than(p, &["tools"]) {
                p.remove(&key);
                dropped.push(format!("tool_plugins.plugins.{plugin_key}.{key}"));
            }
            if remove_if_not_a_map(p, "tools") {
                dropped.push(format!("tool_plugins.plugins.{plugin_key}.tools"));
            }
            let Some(tools) = p.get_mut("tools").and_then(Value::as_object_mut) else {
                continue;
            };
            for key in non_map_keys(tools) {
                tools.remove(&key);
                dropped.push(format!("tool_plugins.plugins.{plugin_key}.tools.{key}"));
            }
            for (tool_id, tstate) in tools.iter_mut() {
                let Some(t) = tstate.as_object_mut() else {
                    continue;
                };
                for key in keys_other_than(t, OVERLAY_TOOL_PLUGIN_LEAVES) {
                    t.remove(&key);
                    dropped.push(format!(
                        "tool_plugins.plugins.{plugin_key}.tools.{tool_id}.{key}"
                    ));
                }
            }
        }
    }
    // A container reduced to `{}` (or `{"plugins": {}}`) contributes nothing to
    // a merge and only noise to a diff; drop the husk.
    let empty = tp_obj.is_empty()
        || (tp_obj.len() == 1
            && tp_obj
                .get("plugins")
                .and_then(Value::as_object)
                .is_some_and(serde_json::Map::is_empty));
    if empty {
        root.remove("tool_plugins");
    }
    dropped
}

/// The fields of a `harness.<id>` row that a project overlay may **not** carry
/// (V40 review finding M-2).
///
/// Two different reasons, both machine scope:
///
/// * `last_seen` / `last_verified` / `auto_verify` are written OUT OF BAND by
///   the transcript tap and the auto-verify worker (`mutate_global_harness`), so
///   a Settings save carrying a window-open snapshot of them would stomp a newer
///   observation — the `prompt_templates` stale-snapshot defect in a different
///   field. `input_profile_status` is the recorded outcome of a manual spike
///   against the CLI *installed on this machine*, in the same family.
/// * `expose_commands` decides whether `run_command` is advertised to a
///   harness — a capability grant, and it was already machine scope before V40
///   (as `tool_plugins.expose_commands_<id>`, stripped by
///   [`strip_overlay_tool_plugins`]). A project config file lives inside the
///   sandbox boundary a confined tool can write to; a boundary a confined
///   process can widen is not a boundary.
///
/// Everything else in the row — `expose_code_audit` and the plugin `ext`
/// block — is per-project, exactly as its pre-V40 spelling was
/// (`code_audit.expose_<id>`, `statusline.enabled`, `claude_local.*`,
/// `offload.opencode_provider{,_auto}`,
/// `offload.injection.opencode_native_gate_enabled`).
const OVERLAY_BANNED_HARNESS_FIELDS: &[&str] = &[
    "last_seen",
    "last_verified",
    "auto_verify",
    "input_profile_status",
    "expose_commands",
];

/// Remove the machine-scope fields of every `harness.<id>` row from an overlay
/// value, returning the dotted names of what was dropped (empty ⇒ the overlay
/// was already clean).
///
/// A deny-list, not an allow-list, and deliberately so — the opposite of
/// [`strip_overlay_tool_plugins`]. The default answer for a `harness` row is
/// "the project may set this": each harness plugin declares its own `ext` keys,
/// so an allow-list here would be a second copy of every plugin's settings
/// schema, and a newly declared field would silently stop being project-settable
/// until someone remembered to add it. The five fields that are NOT the
/// project's are enumerable and stable; see [`OVERLAY_BANNED_HARNESS_FIELDS`].
///
/// **Shape is part of it**, the one thing shared with the tool-plugins strip: a
/// non-object `harness`, or a non-object row inside it, is removed and reported
/// rather than walked past — `deep_merge` scalar-overwrites, and a scalar
/// dropped onto a row would delete the whole subtree under it on the way to a
/// lenient reader that then falls back to defaults.
fn strip_overlay_harness(v: &mut Value) -> Vec<String> {
    let mut dropped: Vec<String> = Vec::new();
    let Some(root) = v.as_object_mut() else {
        return dropped;
    };
    if root.get("harness").is_none() {
        return dropped;
    }
    if remove_if_not_a_map(root, "harness") {
        dropped.push("harness".to_string());
        return dropped;
    }
    let Some(rows) = root.get_mut("harness").and_then(Value::as_object_mut) else {
        return dropped;
    };
    for key in non_map_keys(rows) {
        rows.remove(&key);
        dropped.push(format!("harness.{key}"));
    }
    for (id, row) in rows.iter_mut() {
        // Every survivor of `non_map_keys` is an object.
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        for field in OVERLAY_BANNED_HARNESS_FIELDS {
            if obj.remove(*field).is_some() {
                dropped.push(format!("harness.{id}.{field}"));
            }
        }
    }
    // Rows (and a container) reduced to `{}` contribute nothing to a merge and
    // only noise to a diff; drop the husks.
    rows.retain(|_, row| !row.as_object().is_some_and(serde_json::Map::is_empty));
    if rows.is_empty() {
        root.remove("harness");
    }
    dropped
}

/// The keys of `obj` whose value is not a JSON object — the shape half of the
/// allow-list, collected up front for the same borrow reason as
/// [`keys_other_than`].
fn non_map_keys(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    obj.iter()
        .filter(|(_, v)| !v.is_object())
        .map(|(k, _)| k.clone())
        .collect()
}

/// Remove `key` from `obj` when it is present and is not a JSON object.
/// Returns whether it removed anything, so the caller can name it in `dropped`.
fn remove_if_not_a_map(obj: &mut serde_json::Map<String, Value>, key: &str) -> bool {
    match obj.get(key) {
        Some(v) if !v.is_object() => {
            obj.remove(key);
            true
        }
        _ => false,
    }
}

/// The keys of `obj` that are not in `keep`, collected up front so the caller
/// can remove them without borrowing the map twice.
fn keys_other_than(obj: &serde_json::Map<String, Value>, keep: &[&str]) -> Vec<String> {
    obj.keys()
        .filter(|k| !keep.contains(&k.as_str()))
        .cloned()
        .collect()
}

/// SAVE write-through for the machine-scope halves of `tool_plugins`: copy the
/// live enables, timeouts and path maps onto the on-disk global settings.
/// Returns true when anything changed — the caller only rewrites the file then.
///
/// Field by field rather than whole-block (the one place this differs from
/// [`sync_sandbox_into`]): the live value's `variables`/`parameters` are the
/// PROJECT's, so copying the block wholesale would write one repo's overrides
/// into the machine-wide file and hand them to every other project.
///
/// An entry the global file does not have yet is created, because that is what
/// "machine scope" means for a plugin the user has only just configured — but
/// nothing is ever removed here: a plugin whose file is temporarily missing must
/// keep its state (see [`crate::settings::ToolPluginsSettings`]).
/// Write the MACHINE-SCOPE half of the live per-harness map through to the
/// physical global file — the same fields [`strip_overlay_harness`] keeps out of
/// a project overlay, and for the same reasons. This is the only place they can
/// land, so the two lists have to agree; `the_two_halves_of_harness_scope_agree`
/// asserts they do.
///
/// Of the five, three are **excluded here too**: `last_seen`, `last_verified`
/// and `auto_verify` are written out of band, and the Settings window holds a
/// snapshot taken when it opened. Copying the snapshot's copy of those would be
/// a Settings save silently reverting a version observation — the
/// `prompt_templates` stale-snapshot defect (V14 review, HIGH/data loss) in a
/// different field. They have their own writer, [`mutate_global_harness`].
///
/// `expose_code_audit` and the plugin `ext` block are NOT copied: they are the
/// project's (V40 review M-2), so they ride [`save`]'s overlay diff exactly as
/// their pre-V40 spellings did. Rows for harnesses this build does not know are
/// left exactly as the disk has them.
fn sync_harness_into(disk_global: &mut Settings, current: &Settings) -> bool {
    let mut changed = false;
    for (id, live) in &current.harness {
        let Some(harness) = crate::harness::HarnessId::from_id(id) else {
            // An unregistered id in the live map came from the disk file in the
            // first place (nothing else can create one) and is already there.
            continue;
        };
        let disk = disk_global
            .harness
            .entry(id.clone())
            .or_insert_with(|| {
                changed = true;
                crate::settings::HarnessSettings::defaults_for(harness)
            });
        if disk.expose_commands != live.expose_commands {
            disk.expose_commands = live.expose_commands;
            changed = true;
        }
        if disk.input_profile_status != live.input_profile_status {
            disk.input_profile_status = live.input_profile_status.clone();
            changed = true;
        }
    }
    changed
}

fn sync_tool_plugin_state_into(disk_global: &mut Settings, current: &Settings) -> bool {
    let mut changed = false;
    let cur = &current.tool_plugins;
    if disk_global.tool_plugins.global_paths != cur.global_paths {
        disk_global.tool_plugins.global_paths = cur.global_paths.clone();
        changed = true;
    }
    if disk_global.tool_plugins.project_paths != cur.project_paths {
        disk_global.tool_plugins.project_paths = cur.project_paths.clone();
        changed = true;
    }
    // V38 F-3's two `command`-kind exposure switches used to be synced here.
    // They are `harness[<id>].expose_commands` since V40 Phase B and ride
    // `sync_harness_into` — still machine scope, still the only place a UI
    // toggle of them can land, and now one loop over the registry instead of
    // two named fields.
    for (plugin_key, live) in &cur.plugins {
        let disk = disk_global
            .tool_plugins
            .plugins
            .entry(plugin_key.clone())
            .or_insert_with(|| {
                changed = true;
                crate::settings::PluginState::default()
            });
        if disk.enabled != live.enabled {
            disk.enabled = live.enabled;
            changed = true;
        }
        for (tool_id, live_tool) in &live.tools {
            let disk_tool = disk.tools.entry(tool_id.clone()).or_insert_with(|| {
                changed = true;
                crate::settings::ToolState::default()
            });
            if disk_tool.enabled != live_tool.enabled {
                disk_tool.enabled = live_tool.enabled;
                changed = true;
            }
            if disk_tool.timeout_secs != live_tool.timeout_secs {
                disk_tool.timeout_secs = live_tool.timeout_secs;
                changed = true;
            }
        }
    }
    changed
}

/// SAVE step 2: empty both template arrays on a diff side so the overlay
/// never carries them.
fn strip_offload_templates(v: &mut Value) {
    if let Some(off) = v.get_mut("offload").and_then(Value::as_object_mut) {
        off.insert(
            "server_command_templates".to_string(),
            Value::Array(Vec::new()),
        );
        off.insert(
            "remote_backend_templates".to_string(),
            Value::Array(Vec::new()),
        );
    }
}

// ── V37 F5: the MCP registry is GLOBAL, activation is per-project ───────────
//
// `offload.mcp_servers` and `offload.mcp_categories` are the REGISTRY: which
// servers exist, what they are called, how to reach them, which category they
// sit in. `offload.mcp_activation` is the only per-project surface (contract
// C2) — two `BTreeMap`s of name -> bool that `deep_merge` folds key by key.
//
// Left on the normal diff path the registry would repeat the V26 audit-paths
// field report exactly. The generic `diff` replaces arrays WHOLESALE, so the
// first project in which the user touches anything MCP-shaped pins a snapshot
// of the entire server list in ITS overlay; from then on every global edit —
// a new server, a fixed URL, a rotated token — is invisible in that project,
// with no error and nothing in the UI to suggest why. Worse than the audit
// case, because `mcp_servers` predates V37 and overlays carrying it already
// exist in the wild.
//
// So the same three-step treatment the templates got:
//
//   * LOAD — a legacy overlay's servers/categories are promoted into the
//     global baseline once (by name; an existing global name is never
//     overwritten), then the merged view's arrays are ALWAYS overwritten from
//     the global baseline: overlays carry no registry authority.
//   * SAVE — the live arrays are written through to the PHYSICAL global file
//     and replaced with `[]` on both diff sides, so no new overlay carries a
//     copy.
//
// `mcp_activation` is deliberately NOT in any of this: it is the per-project
// half, and stripping it would leave per-project enablement nowhere to live.
//
// One consequence, stated because it is a real behaviour change: a project
// overlay that carried per-project ACCESS flags (`claude_access` and friends)
// loses them to the global value. That is the intended scope split — the
// access flags are spawn-baked structural grants (see
// `OffloadSettings::any_claude_mcp`), and per-project variation is
// `mcp_activation`'s job.
//
// `load_readonly` is NOT exempt, and that is the one place this differs from
// its two precedents (V38 merge review; the gap is pre-existing on develop and
// was surfaced by the V37 -> V38 merge). `load` promotes and then ENFORCES, so
// an overlay's registry never reaches the app's merged view. `load_readonly`
// does neither, so until this fix a project `.cimp/config.json` carrying
// `offload.mcp_servers` / `mcp_categories` / `mcp_activation` deep-merged
// straight into every read-only consumer — the `cimp --offload-mcp` child and
// `run_command`'s settings read. That is an ENABLE/ACTIVATION WIDENING toward
// the offload child (an overlay-declared server joins the pool the child
// describes, and its URL joins `outbound::EndpointAllowlist`, which is built
// from `offload.mcp_servers`), reachable by anything that can write inside the
// project root. It is not code execution and is not claimed as such.
//
// The fix is a KEY REMOVAL on the overlay value before the merge, deliberately
// NOT `strip_mcp_registry`: that one is the SAVE-side diff normalizer and
// INSERTS empty arrays, which under `deep_merge`'s replace-arrays-wholesale
// rule would erase the global registry rather than ignore the overlay's.
//
// `mcp_activation` goes too, even though `load` legitimately keeps it
// per-project: nothing behind `load_readonly` reads it. The child's registry
// consumers are `tool_scope_summary` (a tool-count phrase built from
// `mcp_servers`) and the SSRF endpoint allowlist; activation is resolved
// in-app, by `offload::mcp_host::effective_enable` over the live `load`
// snapshot. A per-project surface with no reader on this leg is a widening
// vector with no upside.

/// LOAD step 1: promote a legacy overlay's servers and categories into the
/// global baseline, keyed by name (a name already present globally wins).
///
/// Returns true when the caller should persist — which here also covers
/// "promoted nothing, but the overlay still carries a registry key". Unlike the
/// template libraries, `mcp_servers` is an OLD field: an overlay whose copy is
/// merely stale (same names, different flags) promotes nothing yet must still
/// be rewritten in the stripped shape, or it keeps shadowing global edits
/// forever. The rewrite is one-time — `save` strips both keys, so the next load
/// finds nothing to heal.
fn promote_overlay_mcp_registry(global: &mut Settings, overlay: &Value) -> bool {
    let mut changed = false;
    let carries = overlay
        .get("offload")
        .and_then(Value::as_object)
        .is_some_and(|o| o.contains_key("mcp_servers") || o.contains_key("mcp_categories"));
    if let Some(entries) = overlay
        .get("offload")
        .and_then(|o| o.get("mcp_servers"))
        .and_then(Value::as_array)
    {
        for e in entries {
            let Ok(m) = serde_json::from_value::<McpServerConfig>(e.clone()) else {
                continue;
            };
            if !m.name.trim().is_empty()
                && !global.offload.mcp_servers.iter().any(|g| g.name == m.name)
            {
                global.offload.mcp_servers.push(m);
                changed = true;
            }
        }
    }
    if let Some(entries) = overlay
        .get("offload")
        .and_then(|o| o.get("mcp_categories"))
        .and_then(Value::as_array)
    {
        for e in entries {
            let Ok(c) = serde_json::from_value::<McpCategory>(e.clone()) else {
                continue;
            };
            if !c.name.trim().is_empty()
                && !global.offload.mcp_categories.iter().any(|g| g.name == c.name)
            {
                global.offload.mcp_categories.push(c);
                changed = true;
            }
        }
    }
    changed || carries
}

/// LOAD step 2: overwrite the merged view's registry arrays from the
/// (post-promotion) global baseline. `mcp_activation` is untouched.
fn enforce_global_mcp_registry(merged: &mut Value, global: &Settings) {
    let Some(off) = merged.get_mut("offload").and_then(Value::as_object_mut) else {
        return;
    };
    if let Ok(v) = serde_json::to_value(&global.offload.mcp_servers) {
        off.insert("mcp_servers".to_string(), v);
    }
    if let Ok(v) = serde_json::to_value(&global.offload.mcp_categories) {
        off.insert("mcp_categories".to_string(), v);
    }
}

/// SAVE step 1 (pure half of the write-through): copy the live registry arrays
/// onto the on-disk global settings. Returns true when anything changed — the
/// caller only rewrites the physical file then.
fn sync_mcp_registry_into(disk_global: &mut Settings, current: &Settings) -> bool {
    let mut changed = false;
    if disk_global.offload.mcp_servers != current.offload.mcp_servers {
        disk_global.offload.mcp_servers = current.offload.mcp_servers.clone();
        changed = true;
    }
    if disk_global.offload.mcp_categories != current.offload.mcp_categories {
        disk_global.offload.mcp_categories = current.offload.mcp_categories.clone();
        changed = true;
    }
    changed
}

/// LOAD step, read-only readers only: **remove** the registry keys from an
/// overlay value before it is merged.
///
/// Removal, not replacement. [`strip_mcp_registry`] is the save-side normalizer
/// and writes `[]` into both keys; running it on an overlay would hand
/// `deep_merge` an explicit empty array, and `deep_merge` replaces arrays
/// wholesale — so the global registry would be ERASED for the child rather than
/// merely un-widened. Two functions with one name-shape, two jobs; this is the
/// load-side one.
///
/// Returns the keys that were actually present, for tests and for a caller that
/// wants to say so. [`load_readonly`] discards it: a lightweight subprocess has
/// no Events lane, and the app's own [`load`] already promotes and reports.
fn strip_overlay_mcp_registry(v: &mut Value) -> Vec<String> {
    const KEYS: [&str; 3] = ["mcp_servers", "mcp_categories", "mcp_activation"];
    let Some(off) = v.get_mut("offload").and_then(Value::as_object_mut) else {
        return Vec::new();
    };
    KEYS.iter()
        .filter(|k| off.remove(**k).is_some())
        .map(|k| format!("offload.{k}"))
        .collect()
}

/// SAVE step 2: empty both registry arrays on a diff side so the overlay never
/// carries them. `mcp_activation` is left alone — it is the per-project half.
fn strip_mcp_registry(v: &mut Value) {
    if let Some(off) = v.get_mut("offload").and_then(Value::as_object_mut) {
        off.insert("mcp_servers".to_string(), Value::Array(Vec::new()));
        off.insert("mcp_categories".to_string(), Value::Array(Vec::new()));
    }
}

/// Write the diff between `settings` and `global` to the custom overlay
/// file in `launch_cwd`. If the diff is empty, deletes any existing
/// overlay (so a user who reverts every change ends up with a clean
/// directory).
pub fn save(settings: &Settings, launch_cwd: &Path, global: &Settings) -> AppResult<()> {
    let path = custom_path(launch_cwd);

    // Every machine-scope family's write-through, in one walk of
    // [`MACHINE_SCOPED`]: copy the live values onto the PHYSICAL global file
    // (read-modify-write, every other field preserved — the
    // `write_global_prompt_templates` pattern) so every project sees them, then
    // normalize both diff sides below so no overlay pins a copy. This is the
    // ONLY place a Settings-window edit of one of them can land. Best-effort: a
    // failed global write must not block the overlay save (the values stay live
    // in memory and re-sync on the next save).
    if let Ok(gpath) = global_path() {
        if gpath.exists() {
            let mut disk = read_settings_or_default(&gpath);
            if sync_machine_scope_into(&mut disk, settings) {
                if let Err(e) = save_to(&gpath, &disk) {
                    tracing::warn!(error = %e, "settings: machine-scope global write-through failed");
                }
            }
        }
    }

    let mut current = serde_json::to_value(settings)
        .map_err(|e| AppError::Settings(format!("serialize current: {e}")))?;
    let mut baseline = serde_json::to_value(global)
        .map_err(|e| AppError::Settings(format!("serialize global: {e}")))?;
    strip_machine_scope_from_diff(&mut current, &mut baseline);

    match diff(&current, &baseline) {
        Some(delta) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(AppError::Io)?;
            }
            // **Stamp the overlay with the schema it was written in** (V40
            // Phase I, issue #107 item 5). `diff` drops `schema_version`
            // because it always equals the baseline's — which is exactly why an
            // overlay could go stale invisibly: after the global migrated,
            // nothing on disk said which schema the project's keys were in, so
            // `load` could only guess or (as it did until Phase I) skip
            // migrating it. One key, re-stamped on every save, and
            // `migrate_overlay` strips it again before the merge so it never
            // reaches the merged `Settings`.
            let mut delta = delta;
            if let Some(obj) = delta.as_object_mut() {
                obj.insert(
                    "schema_version".to_string(),
                    serde_json::json!(crate::settings::schema::CURRENT_SCHEMA_VERSION),
                );
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

/// F-19: append built-in price rows added since this install's
/// `pricing_seeded_generation`, then advance the watermark. Returns `true`
/// when anything changed (including a watermark-only advance), so the caller
/// knows to rewrite the physical global file.
///
/// Why this exists at all: `read_global_llm_pricing` seeds
/// `default_llm_pricing` **only when the global file is absent**, so shipping a
/// new built-in row reaches fresh installs and nobody else — every existing
/// install goes on pricing that model at $0 with no error, which is how the
/// missing `claude-opus-5` row survived to a release candidate.
///
/// Three properties, each load-bearing:
///
/// * **Append-only.** Existing entries are never touched, so a price the user
///   edited stays edited. That is also why this can't just overwrite the table
///   with `default_llm_pricing`.
/// * **Deleted rows stay deleted.** The watermark — not "is this built-in row
///   missing?" — decides what to add, so a row the user removed is not
///   resurrected on the next launch. This is the same reasoning as
///   `templates_seeded`, generalized from a bool to a counter because the
///   built-in set keeps growing.
/// * **Hand-added rows aren't duplicated.** A user who worked around the
///   missing row by adding it themselves already has the prefix, so the
///   top-up skips it and only advances the watermark. Matching is on
///   `model_prefix` because that is the field cost mode actually resolves
///   against; a row with an empty prefix (the Copilot rows) can never
///   suppress anything, since every such row would otherwise collide.
fn top_up_llm_pricing_if_needed(s: &mut Settings) -> bool {
    if s.pricing_seeded_generation >= PRICING_GENERATION {
        return false;
    }
    let have: std::collections::HashSet<&str> = s
        .llm_pricing
        .iter()
        .map(|r| r.model_prefix.as_str())
        .filter(|p| !p.is_empty())
        .collect();
    let missing: Vec<_> = pricing_rows_since(s.pricing_seeded_generation)
        .into_iter()
        .filter(|r| !have.contains(r.model_prefix.as_str()))
        .collect();
    let added = missing.len();
    s.llm_pricing.extend(missing);
    s.pricing_seeded_generation = PRICING_GENERATION;
    tracing::info!(
        added,
        generation = PRICING_GENERATION,
        "settings: llm pricing topped up"
    );
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
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
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
            path.starts_with(dir) || path.to_str().is_some_and(|s| s.starts_with("/avatar/"))
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
    let order = crate::settings::canonical_ai_tab_order();
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
        tracing::warn!(
            id = id.as_str(),
            "integrity: restored missing AI builtin tab"
        );
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
    /// this way). The older Workbench tab predates the mechanism and keeps
    /// whatever name the file carries.
    sync_name: bool,
}

const RESERVED_TAB_SPECS: &[ReservedTabSpec] = &[
    // The V8-03 "Offload Server" reserved tab is retired (schema v25) — the
    // dashboard lives inside the Tool Activity tab as the "Offload server"
    // section now; the v24 → v25 migration drops old persisted entries.
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
    // The V15 "Graph View" reserved tab is retired (schema v26) — the live
    // force-graph lives inside the Tool Activity tab as the "Graph view"
    // section now; the v25 → v26 migration drops old persisted entries.
    ReservedTabSpec {
        id: TOOL_ACTIVITY_TAB_ID,
        log_name: "Tools",
        flag: "tool_activity_tab",
        enabled: |s| s.ui.tool_activity_tab,
        default_tab: default_tool_activity_tab,
        sync_name: true,
    },
    // #51: the Events tab is ADDITIVE — the Tools row above keeps its feed and
    // its sections, and nothing is retired, so no `RETIRED_TAB_IDS` entry
    // belongs to this change. Existing installs gain it through this reconcile
    // (its `ui.events_tab` flag defaults true and an old file lacking the key
    // deserializes to that default), which is why #51 needs no schema bump.
    ReservedTabSpec {
        id: EVENTS_TAB_ID,
        log_name: "Events",
        flag: "events_tab",
        enabled: |s| s.ui.events_tab,
        default_tab: default_events_tab,
        sync_name: true,
    },
];

/// Retired reserved feature tab ids — their dashboards moved inside other
/// tabs and the ids must never reach the runtime: a surviving entry
/// deserializes as a plain closable Shell tab with no view behind it, which
/// then tries to spawn a PTY. The schema migrations prune them from the
/// *global* file, and V40 Phase I extended the cascade to the per-folder
/// overlay too — but only from [`migration::MIN_OVERLAY_SCHEMA_VERSION`] up:
/// an overlay older than that (or carrying no version at all) is deliberately
/// left unmigrated, because below the floor there are no steps left to run, and
/// it re-introduces the entry through the merge. This list feeds the integrity
/// check's fail-safe prune, which catches every source that survives:
/// below-floor overlays, hand-edits, imported files.
///
/// V42 R9 note: the GLOBAL side of this is now stricter than "the migrations
/// prune them" — a below-floor global file is quarantined and reseeded rather
/// than pruned. The overlay is the leg that still needs the fail-safe.
const RETIRED_TAB_IDS: [&str; 4] = [
    OFFLOAD_SERVER_TAB_ID,
    CODE_QUALITY_TAB_ID,
    GRAPH_VIEW_TAB_ID,
    CODE_AUDIT_TAB_ID,
];

/// Drop every tab whose id is in [`RETIRED_TAB_IDS`]. Returns `true` if
/// anything was removed.
fn drop_retired_tabs(settings: &mut Settings) -> bool {
    let before = settings.tabs.len();
    settings.tabs.retain(|t| !RETIRED_TAB_IDS.contains(&t.id()));
    let changed = settings.tabs.len() != before;
    if changed {
        tracing::warn!("integrity: dropped retired reserved tab entry");
    }
    changed
}

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

/// Every reserved AI tab id — **a view over the registry**, not a list.
///
/// Used by the integrity check's "is this id one of our reserved AI builtins?"
/// loops. V40 Phase B replaced `const AI_BUILTIN_IDS: [&str; 3]`: the fixed
/// arity was the defect, not the literals. A harness registering a tab id
/// would have left it outside the membership check — so its tab would never be
/// forced `builtin: true`, never be restored at its canonical position, and
/// never be dropped when disabled, all silently.
fn ai_builtin_ids() -> Vec<&'static str> {
    crate::harness::registry::canonical_tab_ids()
}

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

    // 0. Empty enabled_ai_tabs is invalid — repair to the FIRST REGISTERED
    //    harness's first built-in tab. V40 Phase B replaced a literal
    //    `[crate::settings::ai_tab_id("claude")]`, which made ONE harness load-bearing for the app
    //    booting at all; Phase E replaced `DEFAULT_HARNESS` with the registry's
    //    own order, because that constant is a wire-compatibility promise about
    //    identity-less loopback bodies (locked decision 22) and not an answer to
    //    "which tab should this install boot with".
    //    Phase I: `AiTabId` is a registry lookup now, so the fallback is the
    //    canonical order's own first entry and there is no literal left to
    //    `unwrap_or`. A build that registers NO harness has no AI tab to repair
    //    to; it leaves the list empty rather than inventing one.
    if settings.enabled_ai_tabs.is_empty() {
        if let Some(fallback) = crate::settings::canonical_ai_tab_order().first().copied() {
            settings.enabled_ai_tabs = vec![fallback];
            changed = true;
            tracing::warn!(
                tab = fallback.as_str(),
                "integrity: enabled_ai_tabs was empty; reset to the default harness's tab"
            );
        }
    }

    // 1. Force builtin: true on every reserved AI id if it exists with
    //    builtin: false. Defends against hand-edits trying to flip the flag.
    for tab in settings.tabs.iter_mut() {
        if ai_builtin_ids().contains(&tab.id()) && !tab.builtin() {
            tab.set_builtin(true);
            changed = true;
            tracing::warn!(
                id = tab.id(),
                "integrity: forced builtin: true on AI builtin"
            );
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

    // 4a. Drop retired reserved feature tabs (offload-server, code-quality,
    //     graph-view, code-audit).
    //     The schema migrations prune these from the global file and (since
    //     V40 Phase I) from an overlay that states a version at or above
    //     `MIN_OVERLAY_SCHEMA_VERSION`. An older or unversioned overlay is
    //     left unmigrated by design, and re-introduces the entry through
    //     the merge, where it
    //     deserializes as a plain Shell tab with no view behind it and tries
    //     to spawn a PTY. Runs before the layout pass so step 5 also scrubs
    //     the id from the layout tree, and the post-repair save rewrites the
    //     overlay without it.
    if drop_retired_tabs(settings) {
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


    // 5. Backend layout sanity. The frontend owns the deep integrity
    //    walk (orphan placement, empty-pane collapse) — it has the tree
    //    helpers. The backend's job here is just to keep the file
    //    deserializable and stop a hand-edit from referring to dead tab
    //    ids: drop tab_ids that don't exist, and clear invalid
    //    `focused_pane_id` so the frontend's leftmost-leaf fallback
    //    kicks in.
    if let Some(layout) = settings.layout.as_mut() {
        let valid_ids: HashSet<&str> = settings.tabs.iter().map(|t| t.id()).collect();
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
    use crate::settings::schema::{CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, OPENCODE_TAB_ID};
    use std::path::PathBuf;

    fn fake_default_shell() -> ShellSpec {
        ShellSpec {
            command: PathBuf::from("/bin/bash"),
            args: vec!["-i".to_string()],
        }
    }

    /// `Settings::default()` with every default-ON reserved feature tab
    /// turned off (currently Workbench + Tool Activity + Events), so tab-count
    /// assertions only see the tabs a test explicitly sets up. A future
    /// default-on reserved tab gets disabled HERE once, not in every test
    /// body. Tests that exercise a specific reserved tab re-enable its flag.
    fn base_test_settings() -> Settings {
        let mut s = Settings::default();
        s.workbench.enabled = false;
        s.ui.tool_activity_tab = false;
        s.ui.events_tab = false;
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
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local")];
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
    }

    /// **The integrity check repairs a harness this build has never heard of**
    /// (V40 Phase I, issue #107 item 1).
    ///
    /// The other half of
    /// `harness::layering::an_unshipped_descriptor_round_trips_through_the_tab_machinery`,
    /// and it lives here because `integrity_check` is private to this module —
    /// a test that reached around that boundary would be asserting about a copy.
    ///
    /// Three claims, and the first two are exactly what review finding M-3 said
    /// a third harness silently lost: an enabled tab no `AiTabId` variant
    /// existed for was never restored (nothing to seed it from), and a disabled
    /// one was never dropped (it was outside the membership check). The third is
    /// the canonical position, which used to be a literal ranking.
    #[test]
    fn the_integrity_check_seeds_and_drops_an_unshipped_harnesss_tab() {
        crate::harness::registry::with_extra_harness(
            &crate::harness::registry::EXTRA_TEST_HARNESS,
            || {
                let zeta = crate::settings::ai_tab_id("zeta");
                // 1. Enabled ⇒ restored, from the descriptor's own row.
                let mut s = base_test_settings();
                s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), zeta];
                assert!(integrity_check(&mut s));
                assert_eq!(s.tabs.len(), 2);
                // 2. …at its CANONICAL position: after every tab the shipped
                //    harnesses declare, because it is declared after them.
                assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
                assert_eq!(s.tabs[1].id(), "zeta");
                assert!(s.tabs[1].builtin());
                // 3. Disabled ⇒ dropped, like any other reserved AI id.
                s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude")];
                assert!(integrity_check(&mut s));
                assert!(s.tabs.iter().all(|t| t.id() != "zeta"));
            },
        );
    }

    #[test]
    fn integrity_seeds_only_claude_local_when_setting_is_claude_local_only() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude-local")];
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
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude")];
        integrity_check(&mut s); // seed the claude tab

        // Simulate a file that upgraded before the `question` slot existed:
        // the slot deserialized to the pure default (disabled, empty text).
        if let Some(TabConfig::AiTool(c)) = s.tabs.iter_mut().find(|t| t.id() == CLAUDE_TAB_ID) {
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
        // Name-independent since schema 38: the backfill reseeds from
        // `default_ai_tab`, whose prose is the `{tab}` placeholder.
        assert_eq!(q.text, "{tab} has a question");
    }

    #[test]
    fn integrity_does_not_clobber_user_customized_question_slot() {
        use crate::settings::schema::{NotificationSlot, TabConfig};
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude")];
        integrity_check(&mut s);

        // User deliberately disabled the slot but kept (non-empty) text.
        if let Some(TabConfig::AiTool(c)) = s.tabs.iter_mut().find(|t| t.id() == CLAUDE_TAB_ID) {
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
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local"), crate::settings::ai_tab_id("opencode")];
        integrity_check(&mut s);
        assert_eq!(s.tabs.len(), 3);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        assert_eq!(s.tabs[2].id(), OPENCODE_TAB_ID);
    }

    #[test]
    fn integrity_no_graph_monitor_tab_when_disabled() {
        let mut s = base_test_settings(); // graph disabled by default
        integrity_check(&mut s);
        assert!(s.tabs.iter().all(|t| t.id() != GRAPH_MONITOR_TAB_ID));
    }

    #[test]
    fn integrity_materializes_graph_monitor_tab_after_ai_builtins() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local")];
        s.graph.enabled = true;
        integrity_check(&mut s);
        // Lands right after the two AI builtins, before any shell tab.
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        assert_eq!(s.tabs[2].id(), GRAPH_MONITOR_TAB_ID);
        // Non-closable: builtin flag forced on.
        assert!(s.tabs[2].builtin());
    }

    #[test]
    fn integrity_removes_graph_monitor_tab_when_disabled() {
        let mut s = base_test_settings();
        s.graph.enabled = true;
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == GRAPH_MONITOR_TAB_ID));
        // Disable and re-run: the tab is pruned.
        s.graph.enabled = false;
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert!(s.tabs.iter().all(|t| t.id() != GRAPH_MONITOR_TAB_ID));
    }

    #[test]
    fn reconcile_reserved_tabs_materializes_and_removes_both_live() {
        let mut s = base_test_settings();
        s.graph.enabled = true;
        s.workbench.enabled = true;
        // The live toggle path uses reconcile_reserved_tabs (not the full
        // integrity pass) to materialize both reserved tabs at once.
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().any(|t| t.id() == GRAPH_MONITOR_TAB_ID));
        assert!(s.tabs.iter().any(|t| t.id() == WORKBENCH_TAB_ID));
        // Idempotent: no flag change → no tab change.
        assert!(!reconcile_reserved_tabs(&mut s));
        // Disabling both prunes both.
        s.graph.enabled = false;
        s.workbench.enabled = false;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().all(|t| t.id() != GRAPH_MONITOR_TAB_ID));
        assert!(s.tabs.iter().all(|t| t.id() != WORKBENCH_TAB_ID));
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
        // Ordering: AI builtins, then Code Graph monitor, then Workbench,
        // then user shells — mirrors the reserved feature tabs'
        // contiguous-leftmost placement in RESERVED_TAB_SPECS order.
        let mut s = Settings::default();
        s.graph.enabled = true;
        integrity_check(&mut s);
        let graph_pos = s
            .tabs
            .iter()
            .position(|t| t.id() == GRAPH_MONITOR_TAB_ID)
            .unwrap();
        let workbench_pos = s
            .tabs
            .iter()
            .position(|t| t.id() == WORKBENCH_TAB_ID)
            .unwrap();
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
        let workbench_pos = s
            .tabs
            .iter()
            .position(|t| t.id() == WORKBENCH_TAB_ID)
            .unwrap();
        let tool_activity_pos = s
            .tabs
            .iter()
            .position(|t| t.id() == TOOL_ACTIVITY_TAB_ID)
            .unwrap();
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

    // ── #51: the Events tab ──────────────────────────────────────────────

    /// Fresh install. `ui.events_tab` defaults true, so the tab is there
    /// without anyone touching a flag — and, because #51 is additive, the Tool
    /// Activity tab is still there beside it, to its left.
    #[test]
    fn integrity_materializes_events_tab_by_default() {
        let mut s = Settings::default();
        integrity_check(&mut s);

        let entry = s
            .tabs
            .iter()
            .find(|t| t.id() == EVENTS_TAB_ID)
            .expect("fresh install has the Events tab");
        assert!(entry.builtin(), "the Events tab is non-closable");

        let tool_activity_pos = s
            .tabs
            .iter()
            .position(|t| t.id() == TOOL_ACTIVITY_TAB_ID)
            .expect("Tool Activity is untouched by #51");
        let events_pos = s.tabs.iter().position(|t| t.id() == EVENTS_TAB_ID).unwrap();
        assert!(tool_activity_pos < events_pos);
    }

    /// **The upgrade path, and the reason #51 needs no schema-version bump.**
    ///
    /// An existing install's settings file was written before `ui.events_tab`
    /// and the `events` tab entry existed. This reconstructs exactly that —
    /// serialize a settled install, delete the key and the tab — and pins that
    /// the tab arrives on the next load while the user's own tabs, their
    /// order, and their layout tree all survive. A migration would have had to
    /// promise the same thing; the integrity check already does.
    #[test]
    fn an_existing_install_gains_the_events_tab_without_losing_its_layout() {
        use crate::settings::schema::LayoutPersisted;

        let mut before = Settings::default();
        integrity_check(&mut before);
        before.tabs.push(default_shell_1_tab(&fake_default_shell()));
        let user_tabs: Vec<String> = before
            .tabs
            .iter()
            .filter(|t| t.id() != EVENTS_TAB_ID)
            .map(|t| t.id().to_string())
            .collect();
        before.layout = Some(LayoutPersisted {
            tree: LayoutNodePersisted::Pane {
                id: "pane-1".to_string(),
                tab_ids: user_tabs.clone(),
                active_tab_id: Some(CLAUDE_TAB_ID.to_string()),
            },
            focused_pane_id: "pane-1".to_string(),
        });

        // Roll the file back to its pre-#51 shape: no `ui.events_tab` key, no
        // `events` entry in `tabs`.
        let mut json = serde_json::to_value(&before).expect("serialize");
        json["ui"]
            .as_object_mut()
            .unwrap()
            .remove("events_tab")
            .expect("the key exists to be removed");
        let tabs = json["tabs"].as_array_mut().unwrap();
        tabs.retain(|t| t["id"] != serde_json::json!(EVENTS_TAB_ID));

        let mut s: Settings = serde_json::from_value(json).expect("a pre-#51 file still loads");
        assert!(
            s.ui.events_tab,
            "a file lacking the key must read as enabled — that is what makes \
             the integrity check, rather than a migration, the upgrade path"
        );
        assert!(s.tabs.iter().all(|t| t.id() != EVENTS_TAB_ID));

        assert!(integrity_check(&mut s));

        assert!(s.tabs.iter().any(|t| t.id() == EVENTS_TAB_ID));
        // Nothing the user had is gone or reordered relative to itself.
        let after: Vec<&str> = s
            .tabs
            .iter()
            .map(|t| t.id())
            .filter(|id| *id != EVENTS_TAB_ID)
            .collect();
        assert_eq!(after, user_tabs, "existing tabs survive, in order");
        // …and the layout still names them all: a materialized tab must not
        // cost the user the arrangement they had.
        match &s.layout.as_ref().unwrap().tree {
            LayoutNodePersisted::Pane { tab_ids, .. } => assert_eq!(*tab_ids, user_tabs),
            other => panic!("layout tree was rewritten: {other:?}"),
        }
    }

    #[test]
    fn reconcile_reserved_tabs_covers_events_live_toggle() {
        // Same shape as the Tool Activity live-toggle test: the Settings
        // window's save path materializes/removes without a restart.
        let mut s = Settings::default();
        integrity_check(&mut s);
        assert!(s.tabs.iter().any(|t| t.id() == EVENTS_TAB_ID));

        s.ui.events_tab = false;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().all(|t| t.id() != EVENTS_TAB_ID));
        // Disabling Events leaves Tool Activity alone — they are two tabs.
        assert!(s.tabs.iter().any(|t| t.id() == TOOL_ACTIVITY_TAB_ID));
        assert!(!reconcile_reserved_tabs(&mut s));

        s.ui.events_tab = true;
        assert!(reconcile_reserved_tabs(&mut s));
        assert!(s.tabs.iter().any(|t| t.id() == EVENTS_TAB_ID));
    }

    /// Nothing is retired by #51, so the id must not be in the retired list —
    /// an entry there would delete the tab on every load.
    #[test]
    fn the_events_tab_is_additive_not_a_replacement() {
        assert!(!RETIRED_TAB_IDS.contains(&EVENTS_TAB_ID));
        assert!(!RETIRED_TAB_IDS.contains(&TOOL_ACTIVITY_TAB_ID));
        assert!(!RETIRED_TAB_IDS.contains(&WORKBENCH_TAB_ID));
    }

    #[test]
    fn integrity_inserts_opencode_between_claude_local_and_user_shell() {
        // User has [claude, claude-local, shell-foo] and now enables
        // opencode. The new tab should land at index 2 (after claude-local,
        // before the shell), not at the end.
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local"), crate::settings::ai_tab_id("opencode")];
        integrity_check(&mut s);
        // Insert a user shell tab to simulate the existing layout.
        s.tabs
            .push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
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
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local")];
        integrity_check(&mut s);
        assert_eq!(s.tabs.len(), 2);

        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude")];
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
        assert_eq!(s.enabled_ai_tabs, vec![crate::settings::ai_tab_id("claude")]);
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
        assert!(s.tabs.iter().all(|t| t.id() != SHELL_DEFAULT_TAB_ID));
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
        s.tabs
            .push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
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
            }));
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
        s.tabs
            .push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
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

    /// A stale per-folder overlay written before the offload-server /
    /// code-quality / graph-view / code-audit retirements re-introduces the
    /// retired tab entry through the merge — the overlay is never schema-migrated (see
    /// `load`), so the schema-version prunes never see it. The integrity
    /// check must drop it (it would otherwise boot as a plain Shell tab with
    /// no view behind it and fail to spawn a PTY), while leaving user shells
    /// untouched.
    #[test]
    fn integrity_drops_retired_reserved_tabs() {
        let mut s = base_test_settings();
        integrity_check(&mut s);
        for (id, name) in [
            (OFFLOAD_SERVER_TAB_ID, "Offload Server"),
            (CODE_QUALITY_TAB_ID, "Code Quality"),
            (GRAPH_VIEW_TAB_ID, "Graph View"),
            (CODE_AUDIT_TAB_ID, "Code Audit"),
            ("shell-user-1", "Build Watch"),
        ] {
            s.tabs
                .push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
                    id: id.to_string(),
                    builtin: id != "shell-user-1",
                    name: name.to_string(),
                    command: "/bin/bash".to_string(),
                    args: vec!["-i".to_string()],
                    cwd: None,
                    env: Default::default(),
                    notifications: Default::default(),
                    theme_override: None,
                    background_override: None,
                }));
        }
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert!(!s.tabs.iter().any(|t| t.id() == OFFLOAD_SERVER_TAB_ID));
        assert!(!s.tabs.iter().any(|t| t.id() == CODE_QUALITY_TAB_ID));
        assert!(!s.tabs.iter().any(|t| t.id() == GRAPH_VIEW_TAB_ID));
        assert!(!s.tabs.iter().any(|t| t.id() == CODE_AUDIT_TAB_ID));
        assert!(s.tabs.iter().any(|t| t.id() == "shell-user-1"));
        // Idempotent: a second pass finds nothing to repair.
        assert!(!integrity_check(&mut s));
    }

    #[test]
    fn v1_2_round_trip() {
        let _shell = fake_default_shell();
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local")];
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
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("claude"), crate::settings::ai_tab_id("claude-local")];
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
            assert!(
                !c.use_local_provider,
                "claude must have use_local_provider=false"
            );
        }
        if let TabConfig::AiTool(c) = &s.tabs[1] {
            assert!(
                c.use_local_provider,
                "claude-local must have use_local_provider=true"
            );
        }
    }

    #[test]
    fn integrity_corrects_use_local_provider_on_opencode() {
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![crate::settings::ai_tab_id("opencode")];
        integrity_check(&mut s);
        // Tamper: opencode → local (it has no local variant; canonical is false).
        if let TabConfig::AiTool(c) = s
            .tabs
            .iter_mut()
            .find(|t| t.id() == OPENCODE_TAB_ID)
            .unwrap()
        {
            c.use_local_provider = true;
        }
        let changed = integrity_check(&mut s);
        assert!(changed);
        if let TabConfig::AiTool(c) = s.tabs.iter().find(|t| t.id() == OPENCODE_TAB_ID).unwrap() {
            assert!(
                !c.use_local_provider,
                "opencode must have use_local_provider=false"
            );
        }
    }

    #[test]
    fn ui_theme_round_trip_and_default() {
        // Default file has ui.theme = "tui" — the built-in theme — with the
        // default blue accent (new installs land here).
        let s = Settings::default();
        assert_eq!(s.ui.theme, "tui");
        assert_eq!(s.ui.tui_accent, "#7aa2f7");

        // Round-trip preserves hand-edited values (here: a user who
        // switched to a disk theme and picked a custom accent).
        let mut s = Settings::default();
        s.ui.theme = "nippon-dark".to_string();
        s.ui.tui_accent = "#d77757".to_string();
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme, "nippon-dark");
        assert_eq!(parsed.ui.tui_accent, "#d77757");

        // A v1.3 file without the `ui` field still parses (serde(default)).
        let v1_3_json = r#"{"tabs":[]}"#;
        let parsed: Settings = serde_json::from_str(v1_3_json).unwrap();
        assert_eq!(parsed.ui.theme, "tui");
        assert_eq!(parsed.ui.tui_accent, "#7aa2f7");
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

    /// V37 contract C2, the reason `McpActivation` is a `BTreeMap` and not a
    /// `Vec`. A project overlay carries only the keys it overrides; every other
    /// project-level entry in the global file must survive the merge. Written
    /// against `deep_merge` directly because that is the function the shape
    /// argument is about — swap either activation half to an array and this
    /// test fails exactly the way production would.
    #[test]
    fn overlay_activation_merges_per_key() {
        let mut base = serde_json::json!({
            "offload": {
                "mcp_activation": {
                    "servers": { "ddg": true, "git": false },
                    "categories": { "research": true },
                },
                "mcp_servers": [{ "name": "ddg" }],
            },
        });
        let overlay = serde_json::json!({
            "offload": { "mcp_activation": { "servers": { "x": false } } },
        });
        deep_merge(&mut base, overlay);
        assert_eq!(
            base["offload"]["mcp_activation"]["servers"],
            serde_json::json!({ "ddg": true, "git": false, "x": false }),
            "an overlay entry must ADD to the map, not replace it"
        );
        // The untouched half and the global-only server list are unaffected.
        assert_eq!(
            base["offload"]["mcp_activation"]["categories"],
            serde_json::json!({ "research": true })
        );
        assert_eq!(
            base["offload"]["mcp_servers"],
            serde_json::json!([{ "name": "ddg" }])
        );

        // An overlay may also FLIP an existing key (that is the override), and
        // it may override a category without disturbing the server half.
        let overlay2 = serde_json::json!({
            "offload": { "mcp_activation": {
                "servers": { "ddg": false },
                "categories": { "research": false },
            } },
        });
        deep_merge(&mut base, overlay2);
        assert_eq!(
            base["offload"]["mcp_activation"]["servers"],
            serde_json::json!({ "ddg": false, "git": false, "x": false })
        );
        assert_eq!(
            base["offload"]["mcp_activation"]["categories"],
            serde_json::json!({ "research": false })
        );
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
        assert_eq!(
            delta,
            serde_json::json!({ "ui": { "theme": "future-light" } })
        );

        // Reverse: apply delta to global, deserialize, confirm we get
        // `customized` back.
        let mut reapplied = g_value.clone();
        deep_merge(&mut reapplied, delta);
        let recovered: Settings = serde_json::from_value(reapplied).unwrap();
        assert_eq!(recovered.ui.theme, "future-light");
    }

    #[test]
    fn stamp_avatar_paths_uses_files_present_in_dir() {
        let dir = std::env::temp_dir().join(format!("cimp_avatars_{}", uuid::Uuid::new_v4()));
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
            s.avatar
                .transition
                .path
                .as_deref()
                .map(|p| p.to_string_lossy().to_string()),
            Some("/avatar/Transition.mp4".to_string())
        );

        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(
            s.avatar.images.idle.as_deref(),
            Some(dir.join("Idle.mp4").as_path())
        );
        assert_eq!(
            s.avatar.images.speaking.as_deref(),
            Some(dir.join("Speaking.mp4").as_path())
        );
        assert!(
            s.avatar.images.listening.is_none(),
            "missing files should not be stamped"
        );
        assert!(s.avatar.images.thinking.is_none());
        assert!(s.avatar.images.error.is_none());
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(dir.join("Transition.mp4").as_path())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_prefers_theme_subfolder() {
        let dir =
            std::env::temp_dir().join(format!("cimp_avatars_themed_{}", uuid::Uuid::new_v4()));
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

        assert_eq!(
            s.avatar.images.idle.as_deref(),
            Some(tui_yellow.join("Idle.mp4").as_path())
        );
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
        let dir = std::env::temp_dir().join(format!("cimp_avatars_flat_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        for f in ["Idle.mp4", "Transition.mp4"] {
            fs::write(dir.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string(); // tui-yellow/ subfolder does not exist
        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(
            s.avatar.images.idle.as_deref(),
            Some(dir.join("Idle.mp4").as_path())
        );
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(dir.join("Transition.mp4").as_path()),
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_noop_when_dir_empty() {
        let dir = std::env::temp_dir().join(format!("cimp_avatars_empty_{}", uuid::Uuid::new_v4()));
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
        let dir = std::env::temp_dir().join(format!("cimp_avatars_ovr_{}", uuid::Uuid::new_v4()));
        let theme = dir.join("tui-yellow");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("Idle.mp4"), b"").unwrap();

        // A genuine override the user picked from elsewhere on disk.
        let custom = std::env::temp_dir().join(format!("cimp_custom_{}.mp4", uuid::Uuid::new_v4()));
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
        let dir =
            std::env::temp_dir().join(format!("cimp_avatars_switch_{}", uuid::Uuid::new_v4()));
        let yellow = dir.join("tui-yellow");
        let purple = dir.join("tui-purple");
        fs::create_dir_all(&yellow).unwrap();
        fs::create_dir_all(&purple).unwrap();
        fs::write(yellow.join("Idle.mp4"), b"").unwrap();
        fs::write(purple.join("Idle.mp4"), b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(
            s.avatar.images.idle.as_deref(),
            Some(yellow.join("Idle.mp4").as_path())
        );

        // The previously-stamped path (inside `dir`) is re-pointed to the new
        // theme, NOT mistaken for a user override. This is the actual bug fix.
        s.ui.theme = "tui-purple".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(
            s.avatar.images.idle.as_deref(),
            Some(purple.join("Idle.mp4").as_path())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_resets_when_new_theme_has_no_files() {
        let dir = std::env::temp_dir().join(format!("cimp_avatars_reset_{}", uuid::Uuid::new_v4()));
        let yellow = dir.join("tui-yellow");
        fs::create_dir_all(&yellow).unwrap();
        fs::write(yellow.join("Idle.mp4"), b"").unwrap();
        fs::write(yellow.join("Transition.mp4"), b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(
            s.avatar.images.idle.as_deref(),
            Some(yellow.join("Idle.mp4").as_path())
        );
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
            s.avatar
                .transition
                .path
                .as_deref()
                .map(|p| p.to_string_lossy().to_string()),
            Some("/avatar/Transition.mp4".to_string())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **The load path migrates the overlay, and the child reader deliberately
    /// does not** (V40 Phase I, issue #107 item 5).
    ///
    /// Structural for the same reason
    /// `every_settings_reader_runs_the_harness_parse_boundary` is: neither
    /// `load` nor `load_readonly` can be called from a unit test without
    /// writing next to the test binary. The behaviour of the thing they call is
    /// covered in `settings::migration`; what this pins is that they call it —
    /// and that the asymmetry between them is deliberate.
    ///
    /// `load_readonly` (the `cimp --offload-mcp` child) migrates NEITHER file:
    /// it reads the global raw, so a v35 global and a v35 overlay are merged
    /// consistently. Migrating only the overlay there would put a v36-shaped
    /// diff on top of a v35-shaped baseline, which is worse than either. Its
    /// contract is no side effects and the app process repairs the file
    /// moments later, so the child reads a stale-but-coherent view.
    /// Newline-agnostic: CI checks this tree out with CRLF.
    #[test]
    fn the_load_path_migrates_the_overlay_and_the_child_reader_does_not() {
        let src = include_str!("persistence.rs");
        let body_of = |sig: &str| {
            let start = src
                .find(sig)
                .unwrap_or_else(|| panic!("`{sig}` is gone — re-point this test"));
            let body = &src[start..];
            &body[..body.find("\n}").unwrap_or(body.len())]
        };
        assert!(
            body_of("pub fn load(").contains("migrate_overlay"),
            "`load` must run the migration chain on the project overlay: a v35-shaped overlay \
             beside a migrated global carries keys nothing reads, and the project's setting \
             stops applying with the file still on disk saying otherwise"
        );
        assert!(
            !body_of("pub fn load_readonly(").contains("migrate_overlay"),
            "`load_readonly` migrates neither file on purpose — see this test's doc comment. If \
             that changes, migrate the GLOBAL there too or the child merges shapes from two \
             different schemas"
        );
        assert!(
            body_of("pub fn save(").contains("schema_version"),
            "`save` must stamp the overlay with the schema it was written in, or `load` has \
             nothing to enter the cascade at once the global file has moved on"
        );
    }

    // ── The migration floor (V42 R9, issue #120) ───────────────────────────

    /// A scratch directory that removes itself, so a floor test can write a real
    /// global file and then look at what ends up beside it.
    struct FloorDir(PathBuf);
    impl FloorDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cimp_{tag}_{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }
        fn settings_json(&self) -> PathBuf {
            self.0.join("settings.json")
        }
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
    impl Drop for FloorDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn floor_shell() -> ShellSpec {
        ShellSpec {
            command: PathBuf::from("/bin/bash"),
            args: vec!["-i".to_string()],
        }
    }

    /// **A file too old to migrate is set aside, not read.**
    ///
    /// Both below-floor shapes at once — an old stamp, and (the case that needed
    /// deciding) no stamp at all. Three claims, and the first is the one that
    /// matters most to the user: their file still exists, byte for byte, at a
    /// path the log names. Then: defaults are in its place, so the app launches.
    /// And nothing of the old file leaked into them.
    #[test]
    fn a_below_floor_global_file_is_moved_aside_intact_and_defaults_reseeded() {
        for (tag, original) in [
            (
                "floor_v20",
                b"{\r\n \"schema_version\":20,\n\t\"tabs\": [] }\n".to_vec(),
            ),
            // Pre-v1.10: `schema_version` did not exist yet. Valid JSON, no
            // stamp, and the shape the deleted `looks_v1` detector recognised.
            (
                "floor_nostamp",
                br#"{"claude_code": {"command": "claude"}}"#.to_vec(),
            ),
        ] {
            let dir = FloorDir::new(tag);
            let path = dir.settings_json();
            fs::write(&path, &original).unwrap();
            let value: Value = serde_json::from_slice(&original).expect("valid JSON, just old");

            let reseeded = reseed_below_floor(&path, &value, &floor_shell())
                .expect("a below-floor file is handled here, not by the cascade");

            let baks = dir.baks();
            assert_eq!(baks.len(), 1, "exactly one quarantine file: {baks:?}");
            assert!(
                baks[0].contains(".outdated."),
                "it says why it was set aside, and it does not say 'corrupted': {baks:?}"
            );
            assert_eq!(
                fs::read(dir.0.join(&baks[0])).unwrap(),
                original,
                "the user's settings file must survive byte for byte — the floor sets it aside, \
                 it never rewrites it and never deletes it"
            );

            // …and the app has something to launch on.
            let on_disk: Settings =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                on_disk.schema_version,
                crate::settings::schema::CURRENT_SCHEMA_VERSION,
                "the reseeded file is stamped CURRENT, so the next launch is an ordinary one"
            );
            assert_eq!(
                serde_json::to_value(&on_disk).unwrap(),
                serde_json::to_value(&reseeded).unwrap(),
                "what was returned to the caller is what was written"
            );
            assert!(
                !on_disk.tabs.is_empty(),
                "seeded defaults, not a bare `Settings::default()`"
            );
        }
    }

    /// The floor retires steps; it does not stop the ladder. A file **at** the
    /// floor is left alone here and goes on to the cascade like any other.
    #[test]
    fn a_global_file_at_or_above_the_floor_is_left_to_the_cascade() {
        let dir = FloorDir::new("floor_ok");
        let path = dir.settings_json();
        for version in [
            migration::MIN_GLOBAL_SCHEMA_VERSION,
            crate::settings::schema::CURRENT_SCHEMA_VERSION as u64,
        ] {
            let original = format!(r#"{{"schema_version": {version}}}"#).into_bytes();
            fs::write(&path, &original).unwrap();
            let value: Value = serde_json::from_slice(&original).unwrap();

            assert!(
                reseed_below_floor(&path, &value, &floor_shell()).is_none(),
                "v{version} is at or above the floor and must reach the migration cascade"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                original,
                "and it must not have been touched on the way"
            );
            assert!(dir.baks().is_empty(), "no quarantine: {:?}", dir.baks());
        }
    }

    /// **A fresh install is not a quarantine case, and the ORDER is what makes
    /// that true.**
    ///
    /// "States no schema version" is below the floor — that is the pre-v1.10
    /// file. A brand-new install states no version either, for the entirely
    /// different reason that it has no file. The two are told apart by position:
    /// `load_global` seeds a missing file and returns before the floor is ever
    /// consulted. Structural because there is no way to reach `load_global` from
    /// a unit test — it resolves its own path from the running exe.
    /// Newline-agnostic: CI checks this tree out with CRLF.
    #[test]
    fn the_fresh_install_branch_runs_before_the_floor() {
        let src = include_str!("persistence.rs");
        let start = src
            .find("fn load_global(")
            .expect("`load_global` is gone — re-point this test");
        let body = &src[start..];
        let body = &body[..body.find("\n}").unwrap_or(body.len())];

        let seed_at = body
            .find("if !path.exists()")
            .expect("`load_global` must seed defaults for a missing file");
        let floor_at = body
            .find("reseed_below_floor")
            .expect("`load_global` must enforce the migration floor, or a file it cannot migrate \
                     is parsed anyway and silently defaulted");
        let migrate_at = body
            .find("migrate_if_needed")
            .expect("`load_global` must still run the cascade");
        assert!(
            seed_at < floor_at,
            "the fresh-install branch must come FIRST: an absent file states no schema version, \
             which is exactly what a pre-v1.10 file looks like, and quarantining a brand-new \
             install would be nonsense"
        );
        assert!(
            floor_at < migrate_at,
            "the floor must come BEFORE the cascade: after it, a below-floor file has already \
             fallen through every remaining detector and been force-stamped as current"
        );
    }

    /// **Tests only** — drop the overlay's schema stamp, asserting it was
    /// there.
    ///
    /// `save` stamps every overlay it writes with the schema it was written in
    /// (V40 Phase I, issue #107 item 5) so `load` can migrate a stale one
    /// instead of guessing. Every test that pins the overlay's exact object
    /// goes through here, which makes each of them a check that the stamp is
    /// written as well as a check of its own subject.
    #[track_caller]
    fn without_schema_stamp(mut v: Value) -> Value {
        let stamp = v.as_object_mut().and_then(|o| o.remove("schema_version"));
        assert_eq!(
            stamp,
            Some(serde_json::json!(crate::settings::schema::CURRENT_SCHEMA_VERSION)),
            "a saved overlay must carry the schema it was written in"
        );
        v
    }

    #[test]
    fn save_writes_overlay_when_diff_nonempty_and_removes_when_empty() {
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        // Use a unique subdir under the system temp root so parallel test
        // runs don't collide. Cleaned up at the end of the test.
        let dir = std::env::temp_dir().join(format!("cimp_test_{}", uuid::Uuid::new_v4()));
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
        assert_eq!(
            without_schema_stamp(parsed),
            serde_json::json!({ "ui": { "theme": "future-light" } })
        );

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

        let dir =
            std::env::temp_dir().join(format!("cimp_checks_dismiss_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let overlay = custom_path(&dir);

        let mut customized = global.clone();
        customized.checks_suggestion_dismissed = true;
        customized.checks_auto_configure = true;
        save(&customized, &dir, &global).unwrap();

        // The diff carries both fields (they differ from the default baseline).
        let text = fs::read_to_string(&overlay).unwrap();
        assert!(
            text.contains("checks_suggestion_dismissed"),
            "overlay: {text}"
        );
        assert!(text.contains("checks_auto_configure"), "overlay: {text}");

        // Reconstitute: merge the overlay back onto the default baseline.
        let mut merged = serde_json::to_value(&global).unwrap();
        let overlay_val: Value = serde_json::from_str(&text).unwrap();
        deep_merge(&mut merged, overlay_val);
        let loaded: Settings = serde_json::from_value(merged).unwrap();
        assert!(
            loaded.checks_suggestion_dismissed,
            "dismissal survives a save→merge roundtrip"
        );
        assert!(loaded.checks_auto_configure);

        // A config carrying neither key defaults both to false — stamped at the
        // migration floor, the oldest file this build still loads (V42 R9
        // rebased it from v21, which is below the floor and never reaches the
        // typed container at all).
        let old: Settings = serde_json::from_str(r#"{"schema_version": 30}"#).unwrap();
        assert!(!old.checks_suggestion_dismissed);
        assert!(!old.checks_auto_configure);

        let _ = fs::remove_dir_all(&dir);
    }

    /// #48, finding **F-12**: the `run_check`-on-a-remote-worker opt-in must
    /// round-trip through the REAL two-file path — global `settings.json` plus
    /// the *sparse* `<project>/.cimp/config.json` overlay — because that is where
    /// a per-project security decision actually lives. Three claims:
    ///
    /// 1. flipping it on in a project writes exactly that one key to the overlay;
    /// 2. merging the overlay back onto the global baseline resolves it `true`;
    /// 3. **dropping the key returns the value to the global** — i.e. to *denied*,
    ///    which is the direction a missing override has to fail in.
    #[test]
    fn run_check_remote_opt_in_persists_through_the_sparse_project_overlay() {
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);
        assert!(
            !global.checks_allow_remote_worker,
            "the global baseline must be denied"
        );

        let dir = std::env::temp_dir().join(format!("cimp_f12_optin_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let overlay = custom_path(&dir);

        let mut customized = global.clone();
        customized.checks_allow_remote_worker = true;
        save(&customized, &dir, &global).unwrap();

        // (1) The sparse overlay carries only the overridden key.
        let text = fs::read_to_string(&overlay).unwrap();
        let overlay_val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            without_schema_stamp(overlay_val.clone()),
            serde_json::json!({ "checks_allow_remote_worker": true }),
            "overlay: {text}"
        );

        // (2) Merged back onto the global baseline it resolves to the opt-in.
        let mut merged = serde_json::to_value(&global).unwrap();
        deep_merge(&mut merged, overlay_val);
        let loaded: Settings = serde_json::from_value(merged).unwrap();
        assert!(loaded.checks_allow_remote_worker);

        // (3) Drop the override ⇒ back to the global value (denied). Saving the
        //     baseline removes the overlay entirely, which is the same thing.
        save(&global, &dir, &global).unwrap();
        assert!(!overlay.exists());
        let bare: Settings = serde_json::from_value(serde_json::to_value(&global).unwrap()).unwrap();
        assert!(
            !bare.checks_allow_remote_worker,
            "a project with no override must fall back to DENIED"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// **V33 (HIGH, 2026-08-18): a project overlay must not be able to widen or
    /// switch off the OS sandbox.**
    ///
    /// `.cimp/config.json` lives INSIDE the boundary the `sandbox` block
    /// configures — the project root is granted FULL access to every sandboxed
    /// child, and [`load_readonly`] deep-merges the overlay on every MCP-child
    /// call. Three claims, one per half of the fix:
    ///
    /// 1. a sandbox edit never lands in an overlay (so nothing pins one there
    ///    for a later child to inherit);
    /// 2. an overlay that carries a `sandbox` block anyway — the shape a
    ///    confined child could write, `enabled: false` plus a `~/.ssh` grant
    ///    row — is stripped before ANY merge;
    /// 3. the write-through is what keeps the setting savable at all, which is
    ///    the thing a plain ban would have broken.
    #[test]
    fn a_project_overlay_cannot_configure_the_sandbox() {
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        let dir = std::env::temp_dir().join(format!("cimp_v33_sbx_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let overlay = custom_path(&dir);

        let mut customized = global.clone();
        customized.sandbox.enabled = true;
        customized.sandbox.extra_grant_dirs = vec!["/opt/toolchains".to_string()];
        // Something that DOES belong in an overlay, so the diff is non-empty
        // and the file exists to be inspected.
        customized.checks_allow_remote_worker = true;
        save(&customized, &dir, &global).unwrap();

        // (1) The overlay carries the project-scoped key and NOTHING sandboxy.
        let text = fs::read_to_string(&overlay).unwrap();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            without_schema_stamp(val),
            serde_json::json!({ "checks_allow_remote_worker": true }),
            "overlay: {text}"
        );
        assert!(!text.contains("sandbox"), "overlay: {text}");

        // (2) A contaminated overlay is stripped before the merge. This is the
        //     exact function `load` and `load_readonly` both call.
        let hostile = r#"{"sandbox":{"enabled":false,"extra_grant_dirs":["/home/me/.ssh"]},
                          "checks_allow_remote_worker":true}"#;
        fs::write(&overlay, hostile).unwrap();
        let mut v = read_overlay(&overlay, false).expect("the overlay parses");
        strip_overlay_banned(&mut v);
        assert_eq!(
            v,
            serde_json::json!({ "checks_allow_remote_worker": true }),
            "a project overlay may not carry `sandbox`"
        );

        // (3) …and the global write-through is where a real edit lands.
        let mut disk = Settings::default();
        assert!(sync_sandbox_into(&mut disk, &customized));
        assert!(disk.sandbox.enabled);
        assert_eq!(disk.sandbox.extra_grant_dirs, customized.sandbox.extra_grant_dirs);
        assert!(
            !sync_sandbox_into(&mut disk, &customized),
            "a no-op edit must not rewrite the global file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// V38: `tool_plugins` splits INSIDE the block — `variables`/`parameters`
    /// are the project's, everything else is the machine's. Unlike `sandbox`,
    /// which can be banned by key, this needs a structured strip; unlike the
    /// audit paths, the retained part is nested three levels down.
    #[test]
    fn a_project_overlay_carries_only_tool_plugin_variables_and_parameters() {
        let mut hostile: Value = serde_json::json!({
            "tool_plugins": {
                "global_paths": { "acme@1.0.0/scan": "C:\\evil\\acme.exe" },
                "project_paths": { "C:\\repo": { "acme@1.0.0/scan": "C:\\evil\\acme.exe" } },
                "future_field": 1,
                "plugins": {
                    "acme@1.0.0": {
                        "enabled": true,
                        "unknown": 1,
                        "tools": {
                            "scan": {
                                "enabled": true,
                                "timeout_secs": 99999,
                                "variables": { "ruleset": "p/ci" },
                                "parameters": ["--exclude", "vendor"]
                            }
                        }
                    }
                }
            },
            "checks_allow_remote_worker": true
        });
        let dropped = strip_overlay_tool_plugins(&mut hostile);

        assert_eq!(
            hostile,
            serde_json::json!({
                "tool_plugins": { "plugins": { "acme@1.0.0": { "tools": { "scan": {
                    "variables": { "ruleset": "p/ci" },
                    "parameters": ["--exclude", "vendor"]
                } } } } },
                "checks_allow_remote_worker": true
            }),
            "only the two project-scope leaves may survive"
        );
        // Every machine-scope field is NAMED, so the Events row can say which.
        for expected in [
            "tool_plugins.global_paths",
            "tool_plugins.project_paths",
            "tool_plugins.future_field",
            "tool_plugins.plugins.acme@1.0.0.enabled",
            "tool_plugins.plugins.acme@1.0.0.unknown",
            "tool_plugins.plugins.acme@1.0.0.tools.scan.enabled",
            "tool_plugins.plugins.acme@1.0.0.tools.scan.timeout_secs",
        ] {
            assert!(
                dropped.contains(&expected.to_string()),
                "`{expected}` was dropped but not reported: {dropped:?}"
            );
        }
        // An ALLOW-list, not a deny-list: a field this build has never heard of
        // (`future_field`, `unknown`) does not get to ride the overlay by
        // default. That is the direction a machine-scope block has to fail in.

        // A clean overlay says nothing at all — no row, no noise.
        let mut clean: Value = serde_json::json!({ "ui": { "theme": "tui" } });
        assert!(strip_overlay_tool_plugins(&mut clean).is_empty());
        assert_eq!(clean, serde_json::json!({ "ui": { "theme": "tui" } }));
    }

    /// **Every reader of a settings file runs the harness parse boundary**
    /// (V40 review finding M-1).
    ///
    /// `read_settings_or_default`'s doc comment claimed it already — "the load
    /// path, the out-of-band readers, the read-modify-write helpers" — and the
    /// load path was the one that did not. `load_global` parsed straight to
    /// `Settings`, `load` parsed the merged value, and `load_readonly` (the
    /// `cimp --offload-mcp` child) did the same. A hand-edited
    /// `harness.claude.ext.statusline = "yes"` therefore reached the launch path
    /// as a string the accessors answer with the DECLARED DEFAULT while the
    /// Settings window rendered the checkbox OFF: the UI saying one thing and
    /// the spawn doing the other, which is the divergence `SettingKind::accepts`
    /// exists to prevent.
    ///
    /// Structural because there is no way to reach `load_global` from a unit
    /// test without writing next to the test binary; the normaliser's own
    /// behaviour is covered in `settings::schema`. Newline-agnostic: CI checks
    /// this tree out with CRLF.
    #[test]
    fn every_settings_reader_runs_the_harness_parse_boundary() {
        let src = include_str!("persistence.rs");
        for sig in [
            "fn load_global(",
            "pub fn load(",
            "pub fn load_readonly(",
            "fn read_settings_or_default(",
        ] {
            let start = src
                .find(sig)
                .unwrap_or_else(|| panic!("`{sig}` is gone — re-point this test"));
            let body = &src[start..];
            let body = &body[..body.find("\n}").unwrap_or(body.len())];
            assert!(
                body.contains("normalize_harness_settings"),
                "`{sig}` must run the harness parse boundary: a declared `ext` key whose stored \
                 value its kind rejects has to be repaired wherever the file was read from, or \
                 the Settings window and the spawn path answer differently"
            );
        }
    }

    /// The behavioural half: a PROJECT overlay's `ext` values go through the
    /// boundary too, because they are merged in after `load_global` healed the
    /// baseline. Mirrors `load`'s own steps (serialize global -> strip -> merge
    /// -> typed parse -> normalize).
    #[test]
    fn a_project_overlay_ext_value_its_kind_rejects_is_reset_to_the_declared_default() {
        let claude = crate::harness::DEFAULT_HARNESS
            .id()
            .expect("DEFAULT_HARNESS is registered");
        let mut global = Settings::default();
        global.normalize_harness_settings();
        // A declared bool key, to prove the boundary reaches the merged value.
        let declared = crate::harness::DEFAULT_HARNESS
            .descriptor()
            .expect("a registered id has a descriptor")
            .plugin
            .settings_schema()
            .iter()
            .find(|f| matches!(f.kind, crate::harness::plugin::SettingKind::Bool))
            .map(|f| f.key)
            .expect("the default harness declares at least one bool setting");

        let mut merged = serde_json::to_value(&global).unwrap();
        let mut overlay = serde_json::json!({
            "harness": { claude: { "ext": {
                declared: "yes",
                "a.key.from.a.newer.build": { "keep": true }
            } } }
        });
        strip_overlay_banned(&mut overlay);
        let _ = strip_overlay_harness(&mut overlay);
        deep_merge(&mut merged, overlay);

        let mut settings: Settings = serde_json::from_value(merged).unwrap();
        assert_eq!(
            settings.harness[claude].ext.get(declared).and_then(Value::as_str),
            Some("yes"),
            "precondition: the un-normalised merge really does carry the bad value"
        );
        assert!(settings.normalize_harness_settings());
        assert_eq!(
            settings.harness[claude].ext.get(declared),
            global.harness[claude].ext.get(declared),
            "a value the declared kind rejects is reset to the declared default"
        );
        // An UNDECLARED key still rides through untouched — a key a newer cImp
        // declares must survive a downgrade.
        assert_eq!(
            settings.harness[claude].ext.get("a.key.from.a.newer.build"),
            Some(&serde_json::json!({ "keep": true }))
        );
    }

    /// V40 review M-2: `harness` splits INSIDE the block too, and V40 Phase B
    /// banned the whole container.
    ///
    /// Five settings that were per-project on develop moved into it —
    /// `statusline.enabled`, `claude_local.*`, `code_audit.expose_<id>`,
    /// `offload.opencode_provider{,_auto}` and
    /// `offload.injection.opencode_native_gate_enabled`. The ban narrowed all
    /// five to machine scope silently: a project's values became unknown keys at
    /// the first post-upgrade launch and the first post-upgrade save deleted
    /// them, with no Events row and no warning. This pins the split as it now
    /// is: the row's out-of-band fields and the `run_command` capability grant
    /// are the machine's; `expose_code_audit` and the plugin `ext` block are the
    /// project's.
    #[test]
    fn a_project_overlay_carries_the_harness_ext_and_not_the_machine_half() {
        let mut hostile: Value = serde_json::json!({
            "harness": {
                "claude": {
                    "expose_commands": true,
                    "expose_code_audit": false,
                    "last_seen": "9.9.9",
                    "last_verified": "9.9.9",
                    "auto_verify": { "at": "2026-01-01T00:00:00Z" },
                    "input_profile_status": "pass",
                    "ext": { "statusline": false, "local.base_url": "http://myproxy:9000" }
                },
                "opencode": { "last_seen": "1.2.3" },
                "scalar": 7
            },
            "checks_allow_remote_worker": true
        });
        let dropped = strip_overlay_harness(&mut hostile);

        assert_eq!(
            hostile,
            serde_json::json!({
                "harness": { "claude": {
                    "expose_code_audit": false,
                    "ext": { "statusline": false, "local.base_url": "http://myproxy:9000" }
                } },
                "checks_allow_remote_worker": true
            }),
            "only the project-scope half of a harness row may survive"
        );
        for expected in [
            "harness.scalar",
            "harness.claude.expose_commands",
            "harness.claude.last_seen",
            "harness.claude.last_verified",
            "harness.claude.auto_verify",
            "harness.claude.input_profile_status",
            "harness.opencode.last_seen",
        ] {
            assert!(
                dropped.contains(&expected.to_string()),
                "`{expected}` was dropped but not reported: {dropped:?}"
            );
        }

        // A clean overlay says nothing at all — no row, no noise.
        let mut clean: Value = serde_json::json!({ "ui": { "theme": "tui" } });
        assert!(strip_overlay_harness(&mut clean).is_empty());
        assert_eq!(clean, serde_json::json!({ "ui": { "theme": "tui" } }));
    }

    /// The other half of M-2, end to end: a project's `harness.<id>.ext` value
    /// reaches the merged settings, WINS over the machine baseline, and is still
    /// there after a save — which is what "per-project, exactly as on develop"
    /// has to mean for `statusline.enabled` and `claude_local.*`.
    #[test]
    fn a_project_overlay_harness_ext_value_wins_and_survives_a_save() {
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);
        let claude = crate::harness::DEFAULT_HARNESS
            .id()
            .expect("DEFAULT_HARNESS is registered");

        let dir = std::env::temp_dir().join(format!("cimp_v40_m2_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut customized = global.clone();
        {
            let row = customized
                .harness
                .entry(claude.to_string())
                .or_insert_with(|| {
                    crate::settings::HarnessSettings::defaults_for(
                        crate::harness::DEFAULT_HARNESS,
                    )
                });
            row.ext.insert("statusline".to_string(), Value::Bool(false));
            row.ext.insert(
                "local.base_url".to_string(),
                Value::String("http://myproxy:9000".into()),
            );
            // Machine scope: must NOT reach the overlay.
            row.expose_commands = !row.expose_commands;
            row.input_profile_status = "pass".to_string();
            row.last_seen = "9.9.9".to_string();
        }
        save(&customized, &dir, &global).unwrap();

        let text = fs::read_to_string(custom_path(&dir)).unwrap();
        let overlay: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            without_schema_stamp(overlay.clone()),
            serde_json::json!({ "harness": { claude: { "ext": {
                "statusline": false,
                "local.base_url": "http://myproxy:9000"
            } } } }),
            "overlay: {text}"
        );

        // Merge it back the way `load` does, and the project's values win.
        let mut merged = serde_json::to_value(&global).unwrap();
        let mut ov = read_overlay(&custom_path(&dir), false).expect("the overlay parses");
        strip_overlay_banned(&mut ov);
        let _ = strip_overlay_harness(&mut ov);
        deep_merge(&mut merged, ov);
        let reloaded: Settings = serde_json::from_value(merged).unwrap();
        let row = &reloaded.harness[claude];
        assert_eq!(row.ext.get("statusline"), Some(&Value::Bool(false)));
        assert_eq!(
            row.ext.get("local.base_url").and_then(Value::as_str),
            Some("http://myproxy:9000")
        );
        // …and the machine half came from the baseline, not the project.
        assert_eq!(row.expose_commands, global.harness[claude].expose_commands);
        assert_eq!(row.last_seen, global.harness[claude].last_seen);

        // Saving the reloaded state again is idempotent: the same overlay.
        save(&reloaded, &dir, &global).unwrap();
        let again = fs::read_to_string(custom_path(&dir)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&again).unwrap(),
            overlay,
            "a second save must not lose the project's `ext` values"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The strip and the write-through are two halves of ONE scope decision, in
    /// two functions, and a field that fell out of both would be unsavable —
    /// edited in the Settings window, kept out of the overlay, never written to
    /// the global file. Every field `sync_harness_into` copies must therefore be
    /// one the overlay strip removes, and nothing project-scoped may ride it.
    #[test]
    fn the_two_halves_of_harness_scope_agree() {
        let src = include_str!("persistence.rs");
        let start = src
            .find("fn sync_harness_into(")
            .expect("`sync_harness_into` is gone — re-point this test");
        let body = &src[start..];
        let body = &body[..body.find("\n}").unwrap_or(body.len())];
        for field in OVERLAY_BANNED_HARNESS_FIELDS {
            let written = body.contains(&format!("disk.{field} ="));
            // The out-of-band three have their own writer (`mutate_global_harness`).
            let out_of_band = ["last_seen", "last_verified", "auto_verify"].contains(field);
            assert!(
                written || out_of_band,
                "`{field}` is stripped from overlays and not written through, so a Settings \
                 edit of it would have nowhere to land. Either give it a write-through or take \
                 it out of `OVERLAY_BANNED_HARNESS_FIELDS`."
            );
        }
        for field in ["expose_code_audit", "ext"] {
            assert!(
                !body.contains(&format!("disk.{field} =")),
                "`{field}` is the project's (V40 review M-2) and must ride the overlay diff, \
                 not the machine-scope write-through"
            );
        }
    }

    /// **The two settings readers must strip the overlay the same way**
    /// (V38 Phase D; re-pointed at the table by V42 R5, issue #116).
    ///
    /// `run_check` is answered from more than one PROCESS: the app (through
    /// [`load`]) and the `cimp --offload-mcp` child (through [`load_readonly`]).
    /// Both resolve the same effective check set through `checks::plugin`, and
    /// a plugin check's command line is rendered from its declared variable
    /// values — which ride the project overlay. If one reader applied a
    /// different rule to `tool_plugins`, the same check would run with this
    /// project's values on one leg and the machine's on the other, with nothing
    /// anywhere to notice. They stay identical by walking ONE TABLE, and by
    /// every family both legs strip structurally naming the SAME function in
    /// both of its cells. Both halves are pinned here.
    ///
    /// The same claim covers the V37 **MCP registry** (V38 merge review). The
    /// two readers do not handle it identically and must not: `load` promotes
    /// an overlay's servers/categories into the global baseline and then
    /// enforces the global arrays over the merged view, healing the file on the
    /// way; `load_readonly` has no side effects to heal with, so it removes the
    /// keys. Since V42 that difference lives in the ROW rather than in either
    /// reader's body. What is pinned here is that NEITHER reader simply merges
    /// them — the state this test was written against, in which a project
    /// overlay's `offload.mcp_servers` reached the `cimp --offload-mcp` child
    /// untouched — and that the read-only leg REMOVES the keys where the diff
    /// leg empties them.
    ///
    /// Newline-agnostic: CI checks this tree out with CRLF.
    #[test]
    fn both_settings_readers_strip_the_overlay_through_the_same_function() {
        let src = include_str!("persistence.rs");
        // (signature, what its body must name)
        let required: [(&str, &[&str]); 2] = [
            (
                "pub fn load(",
                &[
                    "strip_overlay_for_merge",
                    "promote_overlay_into_global",
                    "enforce_global_machine_scope",
                ],
            ),
            (
                "pub fn load_readonly(",
                &["strip_overlay_for_readonly_merge"],
            ),
        ];
        for (sig, needles) in required {
            let start = src
                .find(sig)
                .unwrap_or_else(|| panic!("`{sig}` is gone — re-point this test"));
            let body = &src[start..];
            let end = body.find("\n}").unwrap_or(body.len());
            let body = &body[..end];
            for needle in needles {
                assert!(
                    body.contains(needle),
                    "`{sig}` must reach the machine-scope families through `{needle}`: they are \
                     never authority an overlay can carry, and a reader that merged one of them \
                     straight through would answer differently from the other. Walk \
                     `MACHINE_SCOPED`; do not hand-enumerate families here again"
                );
            }
        }

        // The half a source scan cannot see: where both legs strip a family
        // structurally, they must strip it with the SAME function.
        for row in MACHINE_SCOPED {
            if let (OverlayStrip::Named(on_load), OverlayStrip::Named(on_readonly)) =
                (&row.overlay_strip, &row.readonly_strip)
            {
                assert!(
                    *on_load as *const () == *on_readonly as *const (),
                    "`{}`: the two readers strip it with different functions, so the same check \
                     could run with this project's values on one leg and the machine's on the \
                     other",
                    row.name
                );
            }
        }

        // The load-side removal must never be the SAVE-side normalizer: that
        // one INSERTS `[]`, and `deep_merge` replaces arrays wholesale. Pinned
        // by BEHAVIOUR rather than by the absence of a call — the read-only leg
        // must leave the registry keys ABSENT, the diff leg present-and-empty.
        let mut overlay = serde_json::json!({
            "offload": { "mcp_servers": [{ "name": "attacker" }], "mcp_categories": [] }
        });
        let _ = strip_overlay_for_readonly_merge(&mut overlay);
        assert_eq!(
            overlay,
            serde_json::json!({ "offload": {} }),
            "`load_readonly` must REMOVE the registry keys, not normalize them to `[]` — an \
             empty array in the overlay would erase the global registry through the merge"
        );
        let mut current = serde_json::json!({ "offload": { "mcp_servers": [{ "name": "real" }] } });
        let mut baseline = current.clone();
        strip_machine_scope_from_diff(&mut current, &mut baseline);
        assert_eq!(
            current["offload"]["mcp_servers"],
            serde_json::json!([]),
            "the SAVE side normalizes to `[]` on BOTH sides, so the diff cancels instead of \
             pinning a copy in the overlay"
        );
    }

    /// **The per-leg row order is frozen** (V42 R5).
    ///
    /// The table replaced hand-enumerated call sites, and the three strip legs
    /// reproduce their exact pre-V42 order: the whole-key bans first (a
    /// documented invariant — [`strip_overlay_banned`] runs before the
    /// structured strips), then the structured strips in this order. The
    /// `promote` and `sync` legs are in TABLE order rather than their pre-V42
    /// order, which is only safe because the families own disjoint keys
    /// (`machine_scope_families_own_disjoint_keys`) — this test is where that
    /// re-ordering is recorded, so a future change to it is a decision and not
    /// an accident.
    #[test]
    fn machine_scope_phase_order_is_pinned() {
        fn names(sel: fn(&MachineScopedField) -> bool) -> Vec<&'static str> {
            MACHINE_SCOPED
                .iter()
                .filter(|r| sel(r))
                .map(|r| r.name)
                .collect()
        }
        assert_eq!(
            MACHINE_SCOPED.iter().map(|r| r.name).collect::<Vec<_>>(),
            [
                "llm_pricing",
                "harness_versions",
                "sandbox",
                "offload_templates",
                "tool_plugins",
                "harness",
                "mcp_registry",
            ],
            "a new family belongs here too — this list is the RECORD of the order three legs              run in, so both adding and re-ordering one are decisions someone confirms"
        );
        assert_eq!(
            names(|r| matches!(r.overlay_strip, OverlayStrip::Named(_))),
            ["tool_plugins", "harness"],
            "`load`'s structured strips, in the order `load` ran them before V42"
        );
        assert_eq!(
            names(|r| matches!(r.readonly_strip, OverlayStrip::Named(_))),
            ["tool_plugins", "harness", "mcp_registry"],
            "`load_readonly`'s structured strips, in the order it ran them before V42"
        );
        assert_eq!(
            names(|r| !matches!(
                r.diff_strip,
                OverlayStrip::Nothing | OverlayStrip::Banned
            )),
            ["offload_templates", "tool_plugins", "harness", "mcp_registry"],
            "`save`'s diff normalizers, in the order it ran them before V42"
        );
        assert_eq!(
            names(|r| r.promote.is_some()),
            ["offload_templates", "tool_plugins", "mcp_registry"],
            "`load`'s promoters — table order, not the pre-V42 order (safe: disjoint keys)"
        );
        assert_eq!(
            names(|r| r.enforce.is_some()),
            ["offload_templates", "mcp_registry"],
            "`load`'s enforcers, which run AFTER `deep_merge`"
        );
        assert_eq!(
            names(|r| r.sync.is_some()),
            [
                "sandbox",
                "offload_templates",
                "tool_plugins",
                "harness",
                "mcp_registry",
            ],
            "`save`'s write-throughs — table order, not the pre-V42 order (safe: disjoint keys)"
        );
    }

    /// **No cell of a machine-scope family may be empty by omission** (V42 R5).
    ///
    /// This is the tripwire the sixteen hand-written functions did not have.
    /// The failure mode of a missed cell is silent — machine state leaking into
    /// a portable overlay, or a setting the Settings window can edit and never
    /// save — and it shipped twice (V38 `tool_plugins`, V40 review M-2
    /// `harness`). Every optional cell here is either filled or paired with a
    /// written reason, and the pairing is asserted both ways.
    #[test]
    fn every_machine_scoped_family_fills_or_explains_every_cell() {
        for row in MACHINE_SCOPED {
            let name = row.name;
            assert!(
                !row.keys.is_empty(),
                "`{name}`: a family with no keys cannot be classified"
            );
            // An overlay can never carry it into the APP's merged view: either
            // it is stripped, or the global baseline is promoted-then-enforced
            // over it.
            assert!(
                !matches!(row.overlay_strip, OverlayStrip::Nothing)
                    || (row.promote.is_some() && row.enforce.is_some()),
                "`{name}`: `load` neither strips it nor promotes-and-enforces it, so a project \
                 overlay's copy reaches the merged view"
            );
            // ...nor into a READ-ONLY child's, and an exemption must say why.
            assert_eq!(
                matches!(row.readonly_strip, OverlayStrip::Nothing),
                row.readonly_exempt.is_some(),
                "`{name}`: `readonly_exempt` must be set exactly when `readonly_strip` is \
                 `Nothing` — the MCP registry was exempt by omission until the V38 merge review \
                 found it reaching the `cimp --offload-mcp` child untouched"
            );
            // ...and an edit of it must have somewhere to land.
            assert_eq!(
                row.sync.is_none(),
                row.sync_writer.is_some(),
                "`{name}`: a machine-scope family with neither a `sync` write-through nor a named \
                 out-of-band writer is one the Settings window can edit and then never save"
            );
            // The diff side is always normalized — otherwise the overlay pins a
            // copy the next launch honours.
            assert!(
                !matches!(row.diff_strip, OverlayStrip::Nothing),
                "`{name}`: `save` would write it into the project overlay"
            );
            // A `[]`-writing normalizer on an overlay leg would ERASE the
            // global value through `deep_merge`, not ignore the overlay's.
            assert!(
                !matches!(row.overlay_strip, OverlayStrip::Normalize(_))
                    && !matches!(row.readonly_strip, OverlayStrip::Normalize(_)),
                "`{name}`: a diff-side normalizer is not an overlay-side strip — see \
                 `the_save_side_normalizer_would_erase_the_global_registry_on_the_load_side`"
            );
        }
    }

    /// **The families own disjoint keys** — which is what makes the `promote`
    /// and `sync` legs order-independent, and therefore what makes it safe for
    /// those two legs to run in table order rather than their pre-V42 order
    /// (see `machine_scope_phase_order_is_pinned`).
    #[test]
    fn machine_scope_families_own_disjoint_keys() {
        let owned: Vec<(&str, &str)> = MACHINE_SCOPED
            .iter()
            .flat_map(|r| r.keys.iter().map(move |k| (r.name, *k)))
            .collect();
        for (i, (a_name, a)) in owned.iter().enumerate() {
            for (b_name, b) in &owned[i + 1..] {
                let overlaps = a == b
                    || b.starts_with(&format!("{a}."))
                    || a.starts_with(&format!("{b}."));
                assert!(
                    !overlaps,
                    "`{a_name}` owns `{a}` and `{b_name}` owns `{b}`: overlapping families make \
                     the promote/sync legs order-dependent, and one of them would silently win"
                );
            }
        }
    }

    /// The whole-key bans are a marker on the row plus one wholesale pass; this
    /// keeps the two from drifting apart. A row marked
    /// [`OverlayStrip::Banned`] whose key is not in [`OVERLAY_BANNED_KEYS`]
    /// would not be stripped at all.
    #[test]
    fn the_banned_rows_and_overlay_banned_keys_agree() {
        let banned: Vec<&str> = MACHINE_SCOPED
            .iter()
            .filter(|r| matches!(r.overlay_strip, OverlayStrip::Banned))
            .flat_map(|r| r.keys.iter().copied())
            .collect();
        assert_eq!(
            banned, OVERLAY_BANNED_KEYS,
            "every `Banned` row's keys must be in `OVERLAY_BANNED_KEYS`, in the same order — the \
             marker does not strip anything by itself"
        );
        for row in MACHINE_SCOPED {
            let is_banned = matches!(row.overlay_strip, OverlayStrip::Banned);
            assert_eq!(
                is_banned,
                matches!(row.readonly_strip, OverlayStrip::Banned)
                    && matches!(row.diff_strip, OverlayStrip::Banned),
                "`{}`: a whole-key ban applies to all three legs or to none — a family banned on \
                 one leg and merged on another is the leak this table exists to prevent",
                row.name
            );
        }
    }

    /// Top-level `Settings` keys a project overlay may carry IN FULL — the
    /// other half of the classification `every_top_level_setting_declares_its_scope`
    /// enforces. Not "harmless": several of these have out-of-band global
    /// readers too (`prompt_templates`), but nothing in them is machine scope
    /// in the sense the table means — an overlay's copy is the project's answer
    /// and is honoured.
    const OVERLAY_CARRYABLE_KEYS: &[&str] = &[
        "schema_version",
        "tts",
        "stt",
        "avatar",
        "display",
        "behavior",
        "usage",
        "system_stats",
        "compose",
        "shortcuts",
        "tabs",
        "processing",
        "session",
        "layout",
        "layout_presets",
        "ui",
        "terminal",
        "external_tools",
        "graph",
        "workbench",
        "delegation",
        "checks",
        "checks_auto_configure",
        "checks_suggestion_dismissed",
        "checks_allow_remote_worker",
        "enabled_ai_tabs",
        "logging",
        "prompt_templates",
        "templates_seeded",
        "pricing_seeded_generation",
        "advisor_dismissed",
        "advisor_applied",
        "preview_last_url",
        "preview_allow_remote",
        "code_audit",
    ];

    /// **A new top-level settings key must declare its scope** (V42 R5).
    ///
    /// The point of the table is that the NEXT machine-scope family cannot
    /// forget a cell. This is the step before that: it cannot be added without
    /// anyone noticing it is a family at all. A key that is neither owned by a
    /// [`MACHINE_SCOPED`] row nor listed in [`OVERLAY_CARRYABLE_KEYS`] fails
    /// here, and the author has to answer the question `tool_plugins` and
    /// `harness` were both shipped without answering.
    ///
    /// Granularity is the top-level key. Inside a container the table already
    /// splits, the container's own rule decides a newly added field:
    /// `tool_plugins`' strip is an ALLOW-list (a new field is machine scope by
    /// default — fails safe), `harness`' is a DENY-list (project by default,
    /// deliberately — see [`OVERLAY_BANNED_HARNESS_FIELDS`]), and `offload`
    /// gets its own classification test below because it is the one mixed
    /// container that fails OPEN.
    #[test]
    fn every_top_level_setting_declares_its_scope() {
        let value = serde_json::to_value(Settings::default()).expect("Settings serializes");
        let on_the_wire: Vec<String> = value
            .as_object()
            .expect("Settings is an object")
            .keys()
            .cloned()
            .collect();
        let machine: Vec<&str> = MACHINE_SCOPED
            .iter()
            .flat_map(|r| r.keys.iter())
            .map(|k| k.split('.').next().expect("a non-empty key"))
            .collect();

        for key in &on_the_wire {
            let is_machine = machine.contains(&key.as_str());
            let is_project = OVERLAY_CARRYABLE_KEYS.contains(&key.as_str());
            assert!(
                is_machine != is_project,
                "`{key}` does not declare its scope. Either it is per-install / per-machine state \
                 — add a `MACHINE_SCOPED` row, which forces you to answer the strip, promote, \
                 enforce, sync and diff questions for it — or a project overlay may carry it, in \
                 which case add it to `OVERLAY_CARRYABLE_KEYS`. Machine state that rides the \
                 overlay leaks this machine's paths, tokens and boundaries into a portable file."
            );
        }
        for key in OVERLAY_CARRYABLE_KEYS {
            assert!(
                on_the_wire.iter().any(|k| k == key),
                "`{key}` is listed as overlay-carryable but is not a `Settings` field any more"
            );
        }
        for key in machine {
            assert!(
                on_the_wire.iter().any(|k| k == key),
                "`{key}` is owned by a `MACHINE_SCOPED` row but is not a `Settings` field any more"
            );
        }
    }

    /// `offload` sub-keys a project overlay may carry — see
    /// `every_offload_field_declares_its_scope`.
    const OVERLAY_CARRYABLE_OFFLOAD_KEYS: &[&str] = &[
        "enabled",
        "autostart",
        "inject_guidance",
        "server_command",
        "tools",
        "allowed_roots",
        "command_allowlist",
        "command_policies",
        "mcp_activation",
        "mcp_health_interval_secs",
        "backends",
        "budget_high_water_pct",
        "per_tool_result_token_cap",
        "max_steps",
        "offload_timeout_secs",
        "global_concurrency",
        "max_queue_depth",
        "escalate_partial",
        "session_push",
        "external_fetch_max_calls",
        "external_fetch_max_bytes",
        "detection_signature_enabled",
        "detection_classifier_enabled",
        "detection_classifier_threshold",
        "detection_update_rules_mode",
        "detection_update_interval_hours",
        "detection_update_manifest_url",
        "native_web_visibility",
        "injection",
    ];

    /// **The same question, one level down, for the one container that fails
    /// open** (V42 R5).
    ///
    /// `offload` is the only settings container that holds both project-scoped
    /// fields and machine-scope ones split out by key — and unlike
    /// `tool_plugins` (allow-list strip) a newly added `offload` field rides
    /// the overlay by default. Both of the families that were carved out of it
    /// (`offload_templates`, `mcp_registry`) were found the same way: a user
    /// reporting that a global edit was invisible inside one project. This
    /// makes the next one a test failure instead.
    #[test]
    fn every_offload_field_declares_its_scope() {
        let value = serde_json::to_value(crate::settings::OffloadSettings::default())
            .expect("OffloadSettings serializes");
        let on_the_wire: Vec<String> = value
            .as_object()
            .expect("OffloadSettings is an object")
            .keys()
            .cloned()
            .collect();
        let machine: Vec<&str> = MACHINE_SCOPED
            .iter()
            .flat_map(|r| r.keys.iter())
            .filter_map(|k| k.strip_prefix("offload."))
            .collect();

        for key in &on_the_wire {
            let is_machine = machine.contains(&key.as_str());
            let is_project = OVERLAY_CARRYABLE_OFFLOAD_KEYS.contains(&key.as_str());
            assert!(
                is_machine != is_project,
                "`offload.{key}` does not declare its scope. A global library or registry belongs \
                 in a `MACHINE_SCOPED` row (`diff` replaces arrays WHOLESALE, so the first \
                 project the user touches pins a snapshot of it and every later global edit is \
                 invisible there); anything the project legitimately varies goes in \
                 `OVERLAY_CARRYABLE_OFFLOAD_KEYS`."
            );
        }
        for key in OVERLAY_CARRYABLE_OFFLOAD_KEYS {
            assert!(
                on_the_wire.iter().any(|k| k == key),
                "`offload.{key}` is listed as overlay-carryable but is not a field any more"
            );
        }
        for key in machine {
            assert!(
                on_the_wire.iter().any(|k| k == key),
                "`offload.{key}` is owned by a `MACHINE_SCOPED` row but is not a field any more"
            );
        }
    }

    /// **A project overlay's MCP registry cannot reach a read-only snapshot**
    /// (V38 merge review; the gap is pre-existing on develop).
    ///
    /// Driven through the real steps [`load_readonly`] performs on the values
    /// it has — global to `Value`, strip the overlay, `deep_merge`,
    /// deserialize — rather than through the function itself, which reads the
    /// machine's real global settings path. Same shape as
    /// `a_hostile_overlay_shape_cannot_re_enable_a_disabled_plugin_tool`, and
    /// for the same reason: what is asserted is what a pipeline READS, not what
    /// a helper returns.
    #[test]
    fn an_overlay_mcp_registry_cannot_reach_a_read_only_snapshot() {
        let mut global = Settings::default();
        global.offload.mcp_servers = vec![McpServerConfig {
            name: "ddg".to_string(),
            ..McpServerConfig::default()
        }];
        global.offload.mcp_categories = vec![McpCategory {
            name: "research".to_string(),
            ..McpCategory::default()
        }];
        let mut merged = serde_json::to_value(&global).unwrap();

        // What anything running inside the project root can write.
        let mut overlay: Value = serde_json::json!({
            "offload": {
                "mcp_servers": [{ "name": "attacker", "url": "http://127.0.0.1:9/" }],
                "mcp_categories": [{ "name": "smuggled" }],
                "mcp_activation": { "servers": { "ddg": false } },
                // A legitimately per-project neighbour, to prove the removal is
                // keyed and not a wholesale drop of the `offload` block.
                "enabled": true
            }
        });
        let removed = strip_overlay_mcp_registry(&mut overlay);
        assert_eq!(
            removed,
            vec![
                "offload.mcp_servers".to_string(),
                "offload.mcp_categories".to_string(),
                "offload.mcp_activation".to_string(),
            ]
        );
        deep_merge(&mut merged, overlay);
        let out: Settings = serde_json::from_value(merged).unwrap();

        let names: Vec<&str> = out
            .offload
            .mcp_servers
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["ddg"],
            "an overlay-declared server must not join the pool the offload child describes, \
             nor the SSRF endpoint allowlist built from this array"
        );
        let cats: Vec<&str> = out
            .offload
            .mcp_categories
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cats, vec!["research"]);
        assert!(
            out.offload.mcp_activation.servers.is_empty(),
            "activation is resolved in-app; nothing behind this reader reads it, so an overlay \
             may not set it here either"
        );
        assert!(
            out.offload.enabled,
            "a non-registry `offload` key still merges"
        );
    }

    /// The trap the fix was ordered around: reusing the SAVE-side normalizer on
    /// an overlay would not un-widen the registry, it would ERASE it — because
    /// `deep_merge` replaces arrays wholesale, and `strip_mcp_registry` inserts
    /// `[]` rather than removing the key.
    #[test]
    fn the_save_side_normalizer_would_erase_the_global_registry_on_the_load_side() {
        let mut global = Settings::default();
        global.offload.mcp_servers = vec![McpServerConfig {
            name: "ddg".to_string(),
            ..McpServerConfig::default()
        }];
        let mut merged = serde_json::to_value(&global).unwrap();
        let mut overlay: Value = serde_json::json!({
            "offload": { "mcp_servers": [{ "name": "attacker" }] }
        });
        strip_mcp_registry(&mut overlay); // the WRONG function on this side
        deep_merge(&mut merged, overlay);
        let out: Settings = serde_json::from_value(merged).unwrap();
        assert!(
            out.offload.mcp_servers.is_empty(),
            "this is why `load_readonly` removes the keys instead: normalizing them to `[]` \
             hands the merge an explicit empty array and the machine's registry is gone"
        );
    }

    /// Phase B review, **B-1**: the strip's allow-list covers SHAPE, and the
    /// claim is not about the strip function — it is about what a hostile
    /// `.cimp/config.json` can do to the answers a pipeline reads.
    ///
    /// So this drives the real three steps `load` performs (global → value,
    /// strip the overlay, `deep_merge`, deserialize) and then asks the
    /// **registry** — the one join every Phase C/D consumer goes through —
    /// whether a tool the user disabled came back on, or whether a timeout the
    /// user set moved. A non-object or `null` at any of the four map levels
    /// used to survive the strip, get scalar-merged over the stored object, be
    /// dropped by the lenient reader, and land the registry on its
    /// never-configured default, which is ENABLED.
    #[test]
    fn a_hostile_overlay_shape_cannot_re_enable_a_disabled_plugin_tool() {
        use crate::plugins::{loader::scan_dir, manifest::Provenance, registry};
        use crate::settings::{PluginState, ToolState};
        use std::collections::BTreeMap;

        let dir = std::env::temp_dir().join(format!("cimp_tp_hostile_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [{ "id": "sec", "label": "Security", "tools": ["scan"] }],
              "tools": [{ "id": "scan", "label": "Acme Scan", "kind": "security", "argv": ["{root}"] }]
            }"#,
        )
        .unwrap();
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);

        // The machine's answer: this tool is OFF, with a path and a timeout the
        // user chose. Everything a hostile overlay would want to change.
        let mut global = Settings::default();
        global.tool_plugins.global_paths.insert(
            "acme@1.0.0/scan".to_string(),
            "C:\\bin\\acme.exe".to_string(),
        );
        global.tool_plugins.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: true,
                tools: BTreeMap::from([(
                    "scan".to_string(),
                    ToolState {
                        enabled: false,
                        timeout_secs: Some(60),
                        ..ToolState::default()
                    },
                )]),
            },
        );

        // One hostile overlay per level, each the shape that used to walk past
        // the strip: a scalar and a null at the plugins-map, the plugin-state,
        // the tools-map and the tool-state levels.
        let hostiles: [(&str, Value); 8] = [
            ("plugins-map scalar", serde_json::json!({"plugins": 5})),
            ("plugins-map null", serde_json::json!({"plugins": null})),
            (
                "plugin-state scalar",
                serde_json::json!({"plugins": {"acme@1.0.0": 5}}),
            ),
            (
                "plugin-state null",
                serde_json::json!({"plugins": {"acme@1.0.0": null}}),
            ),
            (
                "tools-map scalar",
                serde_json::json!({"plugins": {"acme@1.0.0": {"tools": 5}}}),
            ),
            (
                "tools-map null",
                serde_json::json!({"plugins": {"acme@1.0.0": {"tools": null}}}),
            ),
            (
                "tool-state scalar",
                serde_json::json!({"plugins": {"acme@1.0.0": {"tools": {"scan": 5}}}}),
            ),
            (
                "tool-state null",
                serde_json::json!({"plugins": {"acme@1.0.0": {"tools": {"scan": null}}}}),
            ),
        ];

        for (what, tool_plugins) in hostiles {
            let mut overlay = serde_json::json!({ "tool_plugins": tool_plugins });
            let dropped = strip_overlay_tool_plugins(&mut overlay);
            assert!(
                !dropped.is_empty(),
                "{what}: a malformed node must be REPORTED, not silently tolerated"
            );

            let mut merged = serde_json::to_value(&global).unwrap();
            deep_merge(&mut merged, overlay);
            let loaded: Settings = serde_json::from_value(merged).unwrap();

            let tools = registry::effective_tools(&set, &loaded.tool_plugins, None);
            let scan = tools
                .iter()
                .find(|t| t.tool_key == "acme@1.0.0/scan")
                .unwrap_or_else(|| panic!("{what}: the tool vanished from the registry"));
            assert!(
                !scan.enabled && !scan.runnable(),
                "{what}: the overlay re-enabled a tool the user switched off"
            );
            assert_eq!(
                scan.timeout_secs,
                Some(60),
                "{what}: the overlay moved a machine-scope timeout"
            );
            assert_eq!(
                scan.path.as_deref(),
                Some("C:\\bin\\acme.exe"),
                "{what}: the overlay moved a machine-scope path"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// The other half: a Settings-window edit to a machine-scope field made
    /// from inside a customized project still lands somewhere — the physical
    /// global file — because the overlay refuses to carry it.
    #[test]
    fn tool_plugin_machine_scope_writes_through_to_the_global_file() {
        use crate::settings::{PluginState, ToolState};
        use std::collections::BTreeMap;

        let mut live = Settings::default();
        live.tool_plugins.global_paths.insert(
            "acme@1.0.0/scan".to_string(),
            "C:\\bin\\acme.exe".to_string(),
        );
        live.tool_plugins.project_paths.insert(
            "C:\\repo".to_string(),
            BTreeMap::from([("acme@1.0.0/scan".to_string(), "D:\\alt.exe".to_string())]),
        );
        live.tool_plugins.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: false,
                tools: BTreeMap::from([(
                    "scan".to_string(),
                    ToolState {
                        enabled: false,
                        timeout_secs: Some(900),
                        // PROJECT scope — must NOT reach the global file, or one
                        // repo's overrides become every repo's defaults.
                        parameters: vec!["--exclude".into(), "vendor".into()],
                        variables: BTreeMap::from([("ruleset".into(), "p/ci".into())]),
                    },
                )]),
            },
        );

        let mut disk = Settings::default();
        assert!(sync_tool_plugin_state_into(&mut disk, &live));
        assert_eq!(disk.tool_plugins.global_paths, live.tool_plugins.global_paths);
        assert_eq!(disk.tool_plugins.project_paths, live.tool_plugins.project_paths);
        let on_disk = &disk.tool_plugins.plugins["acme@1.0.0"];
        assert!(!on_disk.enabled);
        assert!(!on_disk.tools["scan"].enabled);
        assert_eq!(on_disk.tools["scan"].timeout_secs, Some(900));
        assert!(
            on_disk.tools["scan"].parameters.is_empty()
                && on_disk.tools["scan"].variables.is_empty(),
            "project-scope leaves must not be written through to the machine file"
        );
        assert!(
            !sync_tool_plugin_state_into(&mut disk, &live),
            "a no-op edit must not rewrite the global file"
        );
    }

    /// End to end through `save`: the overlay a real save writes carries the
    /// project's variables and none of the machine's facts.
    #[test]
    fn save_keeps_tool_plugin_paths_out_of_the_overlay() {
        use crate::settings::{PluginState, ToolState};
        use std::collections::BTreeMap;

        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        let dir = std::env::temp_dir().join(format!("cimp_tp_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut customized = global.clone();
        customized.tool_plugins.global_paths.insert(
            "acme@1.0.0/scan".to_string(),
            "C:\\bin\\acme.exe".to_string(),
        );
        customized.tool_plugins.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: false,
                tools: BTreeMap::from([(
                    "scan".to_string(),
                    ToolState {
                        enabled: true,
                        timeout_secs: Some(300),
                        parameters: vec!["--fast".into()],
                        variables: BTreeMap::from([("ruleset".into(), "p/ci".into())]),
                    },
                )]),
            },
        );
        save(&customized, &dir, &global).unwrap();

        let text = fs::read_to_string(dir.join(".cimp").join("config.json")).unwrap();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            without_schema_stamp(val),
            serde_json::json!({
                "tool_plugins": { "plugins": { "acme@1.0.0": { "tools": { "scan": {
                    "variables": { "ruleset": "p/ci" },
                    "parameters": ["--fast"]
                } } } } }
            }),
            "overlay: {text}"
        );
        assert!(!text.contains("acme.exe"), "overlay: {text}");
        assert!(!text.contains("timeout_secs"), "overlay: {text}");

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
        assert!(
            canonical.exists(),
            "legacy overlay should be moved into .cimp/"
        );
        assert!(
            !legacy.exists(),
            "legacy overlay should be gone after the move"
        );
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
        assert!(
            !seeded_second,
            "seeding must not re-fire once templates_seeded is true"
        );
        assert!(
            s.prompt_templates.is_empty(),
            "deleted starters must stay deleted"
        );
    }

    // --- F-19: built-in price rows reach EXISTING installs ---------------

    /// A settings file that does not carry this field must read back as
    /// generation 0, not as `PRICING_GENERATION`.
    ///
    /// This is the whole fix in one assertion. `Settings` carries a
    /// container-level `#[serde(default)]`, which fills missing fields from
    /// `Settings::default()` — and that returns the CURRENT generation. Drop
    /// the field-level `#[serde(default = "pricing_generation_none")]` and this
    /// test fails while everything else stays green: every pre-existing install
    /// would read as already-topped-up and silently never receive another
    /// built-in row.
    #[test]
    fn a_settings_file_without_the_watermark_reads_as_generation_zero() {
        // Stamped at the migration floor — the oldest file this build still
        // loads. V42 R9 rebased it from v29, which is below the floor.
        let s: Settings = serde_json::from_str(r#"{"schema_version": 30}"#).unwrap();
        assert_eq!(
            s.pricing_seeded_generation, 0,
            "a file predating the watermark must look like generation 0, or the \
             top-up never runs on the installs that need it"
        );
    }

    /// Generation 0 has to mean "nothing topped up yet", or the serde default
    /// above is indistinguishable from a current install. Compile-time because
    /// it is a property of the constant, not of any run.
    const _: () = assert!(PRICING_GENERATION > 0);

    #[test]
    fn top_up_appends_new_built_in_rows_to_an_existing_table() {
        // An install carrying the pre-Opus-5 table.
        let mut s = Settings {
            pricing_seeded_generation: 0,
            ..Settings::default()
        };
        s.llm_pricing
            .retain(|r| r.model_prefix != "claude-opus-5");
        let before = s.llm_pricing.len();

        assert!(top_up_llm_pricing_if_needed(&mut s));
        assert_eq!(s.pricing_seeded_generation, PRICING_GENERATION);
        assert_eq!(s.llm_pricing.len(), before + 1);

        let added = s
            .llm_pricing
            .iter()
            .find(|r| r.model_prefix == "claude-opus-5")
            .expect("claude-opus-5 row appended");
        assert_eq!((added.input, added.output), (5.0, 25.0));

        // Idempotent: the watermark stops a second pass.
        assert!(!top_up_llm_pricing_if_needed(&mut s));
        assert_eq!(s.llm_pricing.len(), before + 1);
    }

    /// The property that makes this safe to run on every launch: it is a
    /// one-time top-up, not a reconciliation against `default_llm_pricing`.
    #[test]
    fn top_up_does_not_resurrect_a_row_the_user_deleted() {
        let mut s = Settings {
            pricing_seeded_generation: 0,
            ..Settings::default()
        };
        assert!(top_up_llm_pricing_if_needed(&mut s));

        // User deletes it afterwards. It must stay gone.
        s.llm_pricing.retain(|r| r.model_prefix != "claude-opus-5");
        assert!(!top_up_llm_pricing_if_needed(&mut s));
        assert!(
            !s.llm_pricing.iter().any(|r| r.model_prefix == "claude-opus-5"),
            "a deleted price row must stay deleted"
        );
    }

    /// The state the user who reported F-19 is actually in: they worked around
    /// the missing row by adding it by hand, at their own price.
    #[test]
    fn top_up_neither_duplicates_nor_overwrites_a_hand_added_row() {
        let mut s = Settings {
            pricing_seeded_generation: 0,
            ..Settings::default()
        };
        s.llm_pricing
            .retain(|r| r.model_prefix != "claude-opus-5");
        s.llm_pricing.push(LlmPricingModel {
            provider: "Anthropic".to_string(),
            model: "Opus 5 (mine)".to_string(),
            model_prefix: "claude-opus-5".to_string(),
            input: 4.0,
            cache_write: 8.0,
            cache_read: 0.4,
            output: 20.0,
        });
        let before = s.llm_pricing.len();

        // Still returns true — the watermark advances — but adds nothing.
        assert!(top_up_llm_pricing_if_needed(&mut s));
        assert_eq!(s.llm_pricing.len(), before, "no duplicate row");

        let rows: Vec<_> = s
            .llm_pricing
            .iter()
            .filter(|r| r.model_prefix == "claude-opus-5")
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input, 4.0, "a hand-edited price must not be reset");
        assert_eq!(rows[0].model, "Opus 5 (mine)");
    }

    /// The Copilot rows all carry an empty `model_prefix`; that must not make
    /// them collide with each other or suppress a prefixed row.
    #[test]
    fn empty_prefix_rows_never_suppress_a_top_up() {
        let mut s = Settings {
            pricing_seeded_generation: 0,
            ..Settings::default()
        };
        s.llm_pricing.retain(|r| r.model_prefix.is_empty());
        let before = s.llm_pricing.len();
        assert!(before > 0, "the Copilot rows are the empty-prefix ones");

        assert!(top_up_llm_pricing_if_needed(&mut s));
        assert_eq!(s.llm_pricing.len(), before + 1);
    }

    /// A fresh install already has every row, so it starts topped up and the
    /// migration is a no-op for it.
    #[test]
    fn a_fresh_install_starts_at_the_current_generation() {
        let mut s = Settings::default();
        assert_eq!(s.pricing_seeded_generation, PRICING_GENERATION);
        assert!(s.llm_pricing.iter().any(|r| r.model_prefix == "claude-opus-5"));
        assert!(!top_up_llm_pricing_if_needed(&mut s));
    }

    #[test]
    fn write_then_read_global_prompt_templates_round_trips() {
        let dir = std::env::temp_dir().join(format!("cimp_tpl_global_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let templates = vec![
            PromptTemplate {
                name: "a".to_string(),
                body: "body-a".to_string(),
            },
            PromptTemplate {
                name: "b".to_string(),
                body: "body-b".to_string(),
            },
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
            vec![PromptTemplate {
                name: "a".to_string(),
                body: "x".to_string(),
            }],
        )
        .unwrap();

        let after: Settings = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after.ui.theme, "future-light",
            "unrelated field must survive the R-M-W"
        );
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
        assert_eq!(seeded, crate::pricing::default_llm_pricing());
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
        let dir =
            std::env::temp_dir().join(format!("cimp_price_preserve_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut initial = Settings::default();
        initial.ui.theme = "future-light".to_string();
        save_to(&path, &initial).unwrap();

        write_llm_pricing_to(&path, Vec::new()).unwrap();

        let after: Settings = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after.ui.theme, "future-light",
            "unrelated field must survive the R-M-W"
        );
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
        let dir =
            std::env::temp_dir().join(format!("cimp_tpl_project_absent_{}", uuid::Uuid::new_v4()));
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

    // ── Legacy audit-tool config: one-time promotion ──────────────────────

    /// The v33 promotion, and the two rules that make it safe to run on every
    /// launch: it fills only EMPTY slots, and a container the user has already
    /// touched wins over a stale overlay.
    #[test]
    fn legacy_audit_config_promotes_into_empty_container_slots_only() {
        let mut global = base_test_settings();
        // Already configured through the new pane: must survive untouched.
        global.tool_plugins.global_paths.insert(
            "cimp-audit@1/semgrep".to_string(),
            "C:\\global\\semgrep.exe".to_string(),
        );
        let overlay = serde_json::json!({
            "code_audit": { "tools": [
                {
                    "id": "gitleaks",
                    "enabled": false,
                    "path": "P:\\ebin\\gitleaks.exe",
                    "extra_args": ["--redact"],
                    "timeout_secs": 900
                },
                { "id": "semgrep", "path": "P:\\stale\\semgrep.exe", "ruleset": "p/ci" },
                { "id": "pmd", "path": "   ", "ruleset": "   " }
            ] }
        });

        assert!(promote_overlay_audit_config(&mut global, &overlay));

        // The path the container did not have is promoted; the one it had is not.
        assert_eq!(
            global.tool_plugins.global_paths.get("cimp-audit@1/gitleaks"),
            Some(&"P:\\ebin\\gitleaks.exe".to_string())
        );
        assert_eq!(
            global.tool_plugins.global_paths.get("cimp-audit@1/semgrep"),
            Some(&"C:\\global\\semgrep.exe".to_string()),
            "an already-configured machine path must not be overwritten by a stale overlay"
        );

        let tools = &global.tool_plugins.plugins["cimp-audit@1"].tools;
        let gitleaks = &tools["gitleaks"];
        assert!(!gitleaks.enabled);
        assert_eq!(gitleaks.timeout_secs, Some(900));
        assert_eq!(gitleaks.parameters, vec!["--redact".to_string()]);
        assert_eq!(tools["semgrep"].variables["ruleset"], "p/ci");
        // A blank ruleset meant "use the tool's own default", which is the
        // ABSENCE of a value in the container — storing "" would render
        // `-R ""` on the next scan with no way back.
        assert!(!tools.contains_key("pmd"), "a blank-only entry carries nothing");

        // Idempotent: a second pass over the same overlay changes nothing, so a
        // project that is never saved does not re-promote on every launch.
        assert!(!promote_overlay_audit_config(&mut global, &overlay));
    }

    /// **Phase E gate, B-E2.** A legacy overlay is an unvalidated file: a hand
    /// edit, a tool a later build removed, or a hostile `.cimp/config.json` can
    /// name an id this build does not ship. The promotion must drop it, because
    /// the two things it would otherwise write are worse than a lost setting —
    /// a container slot no pane can show or clear, and a MACHINE-WIDE path
    /// keyed on a name nothing resolves, minted from one project's file.
    #[test]
    fn a_legacy_overlay_cannot_promote_an_id_this_build_does_not_ship() {
        let mut global = base_test_settings();
        let overlay = serde_json::json!({
            "code_audit": { "tools": [
                { "id": "not-a-tool", "enabled": true, "path": "P:/evil/x.exe" },
                { "id": "cargo-audit", "enabled": false, "timeout_secs": 900 },
                { "id": "gitleaks", "enabled": false }
            ] }
        });

        // The one real id still promotes — the filter is a filter, not a veto.
        assert!(promote_overlay_audit_config(&mut global, &overlay));
        let tools = &global.tool_plugins.plugins["cimp-audit@1"].tools;
        assert!(!tools["gitleaks"].enabled);

        assert!(!tools.contains_key("not-a-tool"));
        assert!(!tools.contains_key("cargo-audit"), "a tool V23 removed");
        assert!(
            global.tool_plugins.global_paths.is_empty(),
            "a fabricated id must not mint a machine-wide path: {:?}",
            global.tool_plugins.global_paths
        );
    }

    /// An overlay with no legacy block at all is not a promotion.
    #[test]
    fn a_modern_overlay_promotes_nothing() {
        let mut global = base_test_settings();
        let overlay = serde_json::json!({ "ui": { "theme": "tui" } });
        assert!(!promote_overlay_audit_config(&mut global, &overlay));
        assert!(global.tool_plugins.plugins.is_empty());
        assert!(global.tool_plugins.global_paths.is_empty());
    }

    fn cmd_template(name: &str, command: &str) -> ServerCommandTemplate {
        ServerCommandTemplate {
            name: name.to_string(),
            command: command.to_string(),
        }
    }

    #[test]
    fn offload_template_promotion_adds_new_names_only() {
        let mut global = base_test_settings();
        global
            .offload
            .server_command_templates
            .push(cmd_template("qwen", "llama-server --global"));
        let overlay = serde_json::json!({
            "offload": {
                "server_command_templates": [
                    { "name": "qwen", "command": "llama-server --stale" },
                    { "name": "embed", "command": "llama-server --embedding" },
                    { "name": "   ", "command": "llama-server --anon" },
                ],
                "remote_backend_templates": [
                    { "name": "lan-box", "base_url": "http://10.0.0.2:8080", "auth_token": "t" },
                ],
            }
        });

        assert!(promote_overlay_offload_templates(&mut global, &overlay));
        // Existing global name: never overwritten by the overlay copy.
        assert_eq!(
            global.offload.server_command_templates[0].command,
            "llama-server --global"
        );
        // New name: promoted. Whitespace-only name: skipped.
        assert_eq!(global.offload.server_command_templates.len(), 2);
        assert_eq!(global.offload.server_command_templates[1].name, "embed");
        assert_eq!(global.offload.remote_backend_templates.len(), 1);
        assert_eq!(global.offload.remote_backend_templates[0].name, "lan-box");

        // Idempotent: a second pass changes nothing.
        assert!(!promote_overlay_offload_templates(&mut global, &overlay));
    }

    #[test]
    fn merged_offload_templates_come_from_global_not_overlay() {
        let mut global = base_test_settings();
        global
            .offload
            .server_command_templates
            .push(cmd_template("qwen", "llama-server -m qwen.gguf"));

        // Simulate the deep-merge outcome: the overlay's (stale/empty) arrays
        // replaced the global's wholesale.
        let mut merged = serde_json::to_value(&global).unwrap();
        merged["offload"]["server_command_templates"] = serde_json::json!([]);

        enforce_global_offload_templates(&mut merged, &global);
        let settings: Settings = serde_json::from_value(merged).unwrap();
        assert_eq!(
            settings.offload.server_command_templates, global.offload.server_command_templates,
            "the merged view must take template libraries from the global baseline"
        );
    }

    #[test]
    fn overlay_diff_never_carries_offload_templates() {
        // A settings state that differs from global ONLY in a template
        // library must produce NO overlay at all once both sides are
        // stripped — the library lands in the global file via the save()
        // write-through instead.
        let global = base_test_settings();
        let mut current = global.clone();
        current
            .offload
            .server_command_templates
            .push(cmd_template("qwen", "llama-server -m qwen.gguf"));

        let mut cur_v = serde_json::to_value(&current).unwrap();
        let mut base_v = serde_json::to_value(&global).unwrap();
        strip_offload_templates(&mut cur_v);
        strip_offload_templates(&mut base_v);
        assert!(
            diff(&cur_v, &base_v).is_none(),
            "a template-only change must not create an overlay"
        );

        // A real per-project offload difference still diffs — without the
        // template keys riding along.
        let mut current2 = current.clone();
        current2.offload.server_command = "llama-server --project-specific".to_string();
        let mut cur2_v = serde_json::to_value(&current2).unwrap();
        strip_offload_templates(&mut cur2_v);
        let delta = diff(&cur2_v, &base_v).expect("offload change must diff");
        assert!(
            delta["offload"].get("server_command_templates").is_none(),
            "overlay must not carry a template library copy: {delta}"
        );
    }

    #[test]
    fn sync_offload_templates_into_disk_global_reports_changes() {
        let mut disk = base_test_settings();
        let mut live = base_test_settings();
        live.offload
            .server_command_templates
            .push(cmd_template("qwen", "llama-server -m qwen.gguf"));

        assert!(sync_offload_templates_into(&mut disk, &live));
        assert_eq!(
            disk.offload.server_command_templates,
            live.offload.server_command_templates
        );
        // Unchanged on a second sync — the physical file isn't rewritten.
        assert!(!sync_offload_templates_into(&mut disk, &live));

        // Deleting a template in the live settings deletes it globally too.
        live.offload.server_command_templates.clear();
        assert!(sync_offload_templates_into(&mut disk, &live));
        assert!(disk.offload.server_command_templates.is_empty());
    }
    // ── V37 F5: the MCP registry is global, activation is per-project ────────

    fn mcp_server(name: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn mcp_registry_promotion_adds_new_names_only() {
        let mut global = base_test_settings();
        global
            .offload
            .mcp_servers
            .push(mcp_server("ddg", "http://global/mcp"));
        let overlay = serde_json::json!({
            "offload": {
                "mcp_servers": [
                    { "name": "ddg", "url": "http://stale/mcp" },
                    { "name": "context7", "url": "http://c7/mcp" },
                    { "name": "   ", "url": "http://anon/mcp" },
                ],
                "mcp_categories": [
                    { "name": "research", "servers": ["ddg"], "enabled": true },
                ],
            }
        });

        assert!(promote_overlay_mcp_registry(&mut global, &overlay));
        // Existing global name: never overwritten by the overlay copy.
        assert_eq!(global.offload.mcp_servers[0].url, "http://global/mcp");
        // New name promoted; whitespace-only name skipped.
        assert_eq!(global.offload.mcp_servers.len(), 2);
        assert_eq!(global.offload.mcp_servers[1].name, "context7");
        assert_eq!(global.offload.mcp_categories.len(), 1);
        assert_eq!(global.offload.mcp_categories[0].name, "research");

        // Still "persist me" on a second pass, because the overlay still
        // CARRIES the keys — that heal is what stops a stale copy shadowing
        // global edits forever. An overlay with no registry keys asks for
        // nothing.
        assert!(promote_overlay_mcp_registry(&mut global, &overlay));
        let clean = serde_json::json!({ "offload": { "mcp_activation": { "servers": {} } } });
        assert!(!promote_overlay_mcp_registry(&mut global, &clean));
    }

    #[test]
    fn merged_mcp_registry_comes_from_global_not_overlay() {
        let mut global = base_test_settings();
        global
            .offload
            .mcp_servers
            .push(mcp_server("ddg", "http://global/mcp"));

        // Simulate the deep-merge outcome: the overlay's stale array replaced
        // the global's wholesale, and its activation map survived per key.
        let mut merged = serde_json::to_value(&global).unwrap();
        merged["offload"]["mcp_servers"] =
            serde_json::json!([{ "name": "ddg", "url": "http://stale/mcp" }]);
        merged["offload"]["mcp_activation"]["servers"] = serde_json::json!({ "ddg": false });

        enforce_global_mcp_registry(&mut merged, &global);
        let settings: Settings = serde_json::from_value(merged).unwrap();
        assert_eq!(
            settings.offload.mcp_servers, global.offload.mcp_servers,
            "the merged view must take the registry from the global baseline"
        );
        // The per-project half is untouched — it is the ONLY per-project half.
        assert_eq!(
            settings.offload.mcp_activation.servers.get("ddg"),
            Some(&false),
            "activation is per-project and must survive"
        );
    }

    #[test]
    fn overlay_diff_never_carries_mcp_registry() {
        // A settings state that differs from global ONLY in the registry must
        // produce NO overlay once both sides are stripped — the registry lands
        // in the global file via the save() write-through instead.
        let global = base_test_settings();
        let mut current = global.clone();
        current
            .offload
            .mcp_servers
            .push(mcp_server("ddg", "http://x/mcp"));
        current.offload.mcp_categories.push(McpCategory {
            name: "research".into(),
            servers: vec!["ddg".into()],
            enabled: true,
        });

        let mut cur_v = serde_json::to_value(&current).unwrap();
        let mut base_v = serde_json::to_value(&global).unwrap();
        strip_mcp_registry(&mut cur_v);
        strip_mcp_registry(&mut base_v);
        assert!(
            diff(&cur_v, &base_v).is_none(),
            "a registry-only change must not create an overlay"
        );

        // The per-project half still diffs — and does so WITHOUT dragging a
        // copy of the registry along.
        let mut current2 = current.clone();
        current2
            .offload
            .mcp_activation
            .servers
            .insert("ddg".into(), false);
        let mut cur2_v = serde_json::to_value(&current2).unwrap();
        strip_mcp_registry(&mut cur2_v);
        let delta = diff(&cur2_v, &base_v).expect("an activation override must diff");
        assert_eq!(
            delta["offload"]["mcp_activation"]["servers"],
            serde_json::json!({ "ddg": false })
        );
        for key in ["mcp_servers", "mcp_categories"] {
            assert!(
                delta["offload"].get(key).is_none(),
                "overlay must not carry {key}: {delta}"
            );
        }
    }

    #[test]
    fn sync_mcp_registry_into_disk_global_reports_changes() {
        // The other half of the write-through: an edit made while a project
        // overlay is in play lands in the PHYSICAL global file, which is what
        // makes it visible from every other project.
        let mut disk = base_test_settings();
        let mut live = base_test_settings();
        live.offload
            .mcp_servers
            .push(mcp_server("ddg", "http://x/mcp"));

        assert!(sync_mcp_registry_into(&mut disk, &live));
        assert_eq!(disk.offload.mcp_servers, live.offload.mcp_servers);
        // Unchanged on a second sync — the physical file isn't rewritten.
        assert!(!sync_mcp_registry_into(&mut disk, &live));

        // A field edit inside an existing row counts as a change (this is what
        // `PartialEq` on `McpServerConfig` is for).
        live.offload.mcp_servers[0].url = "http://y/mcp".into();
        assert!(sync_mcp_registry_into(&mut disk, &live));
        assert_eq!(disk.offload.mcp_servers[0].url, "http://y/mcp");

        // Categories ride the same write-through, and a deletion propagates.
        live.offload.mcp_categories.push(McpCategory {
            name: "research".into(),
            servers: vec!["ddg".into()],
            enabled: true,
        });
        assert!(sync_mcp_registry_into(&mut disk, &live));
        live.offload.mcp_servers.clear();
        assert!(sync_mcp_registry_into(&mut disk, &live));
        assert!(disk.offload.mcp_servers.is_empty());
    }
}

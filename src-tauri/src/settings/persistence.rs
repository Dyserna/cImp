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
use crate::settings::schema::{
    default_ai_tab, default_events_tab, default_graph_monitor_tab,
    default_shell_1_tab, default_tool_activity_tab, default_workbench_tab, pricing_rows_since,
    starter_prompt_templates, AiTabId, HarnessVersions, LayoutNodePersisted, LlmPricingModel,
    McpCategory, McpServerConfig, PromptTemplate, RemoteBackendTemplate, ServerCommandTemplate,
    Settings, TabConfig,
    CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, CODE_AUDIT_TAB_ID, CODE_QUALITY_TAB_ID, EVENTS_TAB_ID,
    GRAPH_MONITOR_TAB_ID, GRAPH_VIEW_TAB_ID, OFFLOAD_SERVER_TAB_ID, OPENCODE_TAB_ID,
    PRICING_GENERATION, SHELL_DEFAULT_TAB_ID, TOOL_ACTIVITY_TAB_ID, WORKBENCH_TAB_ID,
};
use crate::settings::write_atomic;
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
    let overlay_path = overlay_read_path(launch_cwd);
    let overlay_value = read_overlay(&overlay_path, true).map(|mut v| {
        // Per-install fields never belong in an overlay (see
        // `OVERLAY_BANNED_KEYS`) — drop them before the merge so an overlay
        // contaminated by a pre-guard version can't shadow the global file.
        strip_overlay_banned(&mut v);
        // V38: `tool_plugins` cannot be banned wholesale (two of its leaves are
        // genuinely per-project), so it gets a structured strip — and unlike the
        // key ban, this one SAYS SO. A hand-edited config that sets a binary
        // path per repo is a reasonable thing to try and a silent no-op is how
        // that becomes "cImp ignores my config" an hour later.
        crate::plugins::events::record_overlay_strip(
            &overlay_path.display().to_string(),
            &strip_overlay_tool_plugins(&mut v),
        );
        v
    });

    // 2b. Promote legacy overlay scanner paths (empty slots only) and
    //     offload template libraries (new names only) into the global
    //     baseline — see the machine-scope notes above
    //     `promote_overlay_audit_paths` / `promote_overlay_offload_templates`.
    //     Persisted below via the post-load `save`, which also rewrites the
    //     overlay in the stripped shape.
    let promoted = overlay_value.as_ref().is_some_and(|ov| {
        let paths = promote_overlay_audit_config(&mut global, ov);
        let templates = promote_overlay_offload_templates(&mut global, ov);
        let registry = promote_overlay_mcp_registry(&mut global, ov);
        paths || templates || registry
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
        // 3b. Paths and template libraries always come from the
        //     (post-promotion) global baseline — an overlay's copies are
        //     legacy data, not authority.
        enforce_global_offload_templates(&mut merged, &global);
        enforce_global_mcp_registry(&mut merged, &global);
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
        strip_overlay_banned(&mut overlay);
        // V38, and NOT optional here: the children this serves are the Phase C/D
        // consumers that resolve a plugin tool's binary path and enable state,
        // and they run INSIDE the sandbox boundary whose writable area holds
        // this very file. No Events row — a lightweight subprocess has no lane
        // to speak into, and the app's own `load` already reported it.
        let _ = strip_overlay_tool_plugins(&mut overlay);
        // V37 registry, same reason and the SAME asymmetry made explicit: `load`
        // promotes an overlay's servers/categories into the global baseline and
        // then enforces the global arrays over the merged view, so an overlay
        // holds no registry authority there. This reader does neither, so it
        // removes the keys outright — see the block comment above
        // `promote_overlay_mcp_registry` for why removal and not
        // `strip_mcp_registry`.
        let _ = strip_overlay_mcp_registry(&mut overlay);
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
///
/// V35 Phase F: a **changed** `claude_last_seen` is also the first of the two
/// auto-verify triggers (the other is the startup check). It fires from here
/// rather than from the tap because this is the one place the observation is
/// actually recorded — a caller-side trigger would miss the hand-edit and
/// spawn-time paths, and would fire on the no-op re-observations this function
/// exists to swallow. The call is non-blocking (it spawns a detached worker) so
/// the tap is never delayed by a probe.
pub fn note_harness_version(harness: &str, version: &str) {
    let version = version.trim();
    if version.is_empty() {
        return;
    }
    // V40 Phase A: a version note for a harness nobody registered is dropped,
    // loudly enough to find in a log rather than silently. The two field names
    // are still Claude's and OpenCode's — locked decision 5 turns them into a
    // map in Phase B — so the writes stay here, but the DISPATCH is the
    // registry's and an unknown id can no longer land on a `_ => {}` that reads
    // like an intentional no-op.
    let Some(id) = crate::harness::HarnessId::from_id(harness).and_then(|h| h.id()) else {
        tracing::debug!(harness, "version note for an unregistered harness; dropped");
        return;
    };
    let mut claude_changed = false;
    let res = mutate_global_harness_versions(|hv| match id {
        "claude" => {
            claude_changed = hv.claude_last_seen != version;
            hv.claude_last_seen = version.to_string();
        }
        "opencode" => hv.opencode_last_seen = version.to_string(),
        _ => {}
    });
    if let Err(e) = res {
        tracing::warn!("failed to record {harness} version {version}: {e}");
        return;
    }
    if claude_changed {
        crate::harness::verify::on_claude_version_changed();
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
    // F-19: same reasoning — the physical global file is the only place the
    // price table lives, so the top-up has to run here rather than against the
    // merged per-project `Settings`.
    let priced = top_up_llm_pricing_if_needed(&mut typed);

    if migrated || seeded || priced {
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
    // V38 F-3: the two `command`-kind exposure switches. Machine scope like the
    // enables above, so this is the ONLY place a UI toggle of them can land —
    // the overlay strip (an allow-list) drops them from a project diff, and
    // without this line the checkbox would flip in memory and be gone on the
    // next launch.
    if disk_global.tool_plugins.expose_commands_claude != cur.expose_commands_claude {
        disk_global.tool_plugins.expose_commands_claude = cur.expose_commands_claude;
        changed = true;
    }
    if disk_global.tool_plugins.expose_commands_opencode != cur.expose_commands_opencode {
        disk_global.tool_plugins.expose_commands_opencode = cur.expose_commands_opencode;
        changed = true;
    }
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

    // The offload template libraries are machine-scope:
    // write them through to the PHYSICAL global file (read-modify-write,
    // every other field preserved — the `write_global_prompt_templates`
    // pattern) so every project sees them, then normalize both diff sides
    // below so no overlay pins a copy. Best-effort: a failed global write
    // must not block the overlay save (the values stay live in memory and
    // re-sync on the next save).
    if let Ok(gpath) = global_path() {
        if gpath.exists() {
            let mut disk = read_settings_or_default(&gpath);
            let templates_changed = sync_offload_templates_into(&mut disk, settings);
            // V37 F5: the MCP registry is global; only `mcp_activation` varies
            // per project.
            let registry_changed = sync_mcp_registry_into(&mut disk, settings);
            // V33: `sandbox` is banned from overlays (it configures a boundary
            // whose own writable area holds the overlay file), so the global
            // file is the ONLY place a sandbox edit can land.
            let sandbox_changed = sync_sandbox_into(&mut disk, settings);
            // V38: the machine-scope halves of `tool_plugins` (enables,
            // timeouts, both path maps). The overlay carries only the per-tool
            // `variables`/`parameters`, so this is the only place the rest can
            // land — see the block comment above `strip_overlay_tool_plugins`.
            let plugins_changed = sync_tool_plugin_state_into(&mut disk, settings);
            if templates_changed || registry_changed || sandbox_changed || plugins_changed {
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
    strip_overlay_banned(&mut current);
    strip_overlay_banned(&mut baseline);
    strip_offload_templates(&mut current);
    strip_offload_templates(&mut baseline);
    // Both sides, identically: what remains under `tool_plugins` on either side
    // is only `variables`/`parameters`, so the diff can express a project's
    // overrides and nothing else. Return values ignored — a strip of OUR OWN
    // serialized value is not a user's hand edit, so there is nothing to warn
    // about; the load path is where a warning belongs.
    let _ = strip_overlay_tool_plugins(&mut current);
    let _ = strip_overlay_tool_plugins(&mut baseline);
    strip_mcp_registry(&mut current);
    strip_mcp_registry(&mut baseline);

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
    // The V23 "Code Audit" reserved tab is retired (schema v27) — its
    // Security | Quality panels live inside the Tool Activity tab as the
    // "Code audit" section now; the v26 → v27 migration drops old persisted
    // entries.
    // The V25 "Code Quality" reserved tab is retired (schema v23) — the
    // Quality view lives inside the Code Audit surface as a sub-tab now; the
    // v22 → v23 migration drops old persisted entries.
];

/// Retired reserved feature tab ids — their dashboards moved inside other
/// tabs and the ids must never reach the runtime: a surviving entry
/// deserializes as a plain closable Shell tab with no view behind it, which
/// then tries to spawn a PTY. The schema migrations prune them from the
/// *global* file, but the per-folder overlay is deliberately never migrated
/// (see `load`), so an overlay written by an older version re-introduces the
/// entry through the merge. This list feeds the integrity check's fail-safe
/// prune, which catches every source: overlays, hand-edits, imported files.
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

/// All three reserved AI tab ids. Used by the integrity check's "is this
/// id one of our reserved AI builtins?" loops; a single source of truth
/// keeps the `ai_builtins` membership check, the `use_local_provider`
/// expectation table, and the drop-disabled-tab pass in sync.
const AI_BUILTIN_IDS: [&str; 3] = [CLAUDE_TAB_ID, CLAUDE_LOCAL_TAB_ID, OPENCODE_TAB_ID];

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
    //     The global-file schema migrations prune these, but the per-folder
    //     overlay is never migrated (see `load`) — an overlay written by an
    //     older version re-introduces the entry through the merge, where it
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
        assert_eq!(q.text, "Claude has a question");
    }

    #[test]
    fn integrity_does_not_clobber_user_customized_question_slot() {
        use crate::settings::schema::{NotificationSlot, TabConfig};
        let mut s = base_test_settings();
        s.enabled_ai_tabs = vec![AiTabId::Claude];
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
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal, AiTabId::OpenCode];
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
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
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
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal, AiTabId::OpenCode];
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
        s.enabled_ai_tabs = vec![AiTabId::OpenCode];
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
            parsed,
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

        // A config predating Phase D (neither key) defaults both to false.
        let old: Settings = serde_json::from_str(r#"{"schema_version": 21}"#).unwrap();
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
            overlay_val,
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
            val,
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

    /// **The two settings readers must strip the overlay the same way**
    /// (V38 Phase D).
    ///
    /// `run_check` is answered from more than one PROCESS: the app (through
    /// [`load`]) and the `cimp --offload-mcp` child (through [`load_readonly`]).
    /// Both resolve the same effective check set through `checks::plugin`, and
    /// a plugin check's command line is rendered from its declared variable
    /// values — which ride the project overlay. If one reader applied a
    /// different rule to `tool_plugins`, the same check would run with this
    /// project's values on one leg and the machine's on the other, with nothing
    /// anywhere to notice. They stay identical by calling ONE function; this
    /// pins that they still do, at the only level a test can see it without a
    /// real global settings file on disk.
    ///
    /// The same claim covers the V37 **MCP registry** (V38 merge review). The
    /// two readers do not handle it identically and must not: `load` promotes
    /// an overlay's servers/categories into the global baseline and then
    /// enforces the global arrays over the merged view, healing the file on the
    /// way; `load_readonly` has no side effects to heal with, so it removes the
    /// keys. What is pinned here is that NEITHER reader simply merges them —
    /// the state this test was written against, in which a project overlay's
    /// `offload.mcp_servers` reached the `cimp --offload-mcp` child untouched.
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
                    "strip_overlay_tool_plugins",
                    "promote_overlay_mcp_registry",
                    "enforce_global_mcp_registry",
                ],
            ),
            (
                "pub fn load_readonly(",
                &["strip_overlay_tool_plugins", "strip_overlay_mcp_registry"],
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
                    "`{sig}` must name `{needle}`: the machine-scope blocks (`tool_plugins`, the \
                     MCP registry) are never authority an overlay can carry, and a reader that \
                     merged one of them straight through would answer differently from the other"
                );
            }
        }
        // The load-side removal must never be the SAVE-side normalizer: that
        // one INSERTS `[]`, and `deep_merge` replaces arrays wholesale.
        let start = src.find("pub fn load_readonly(").unwrap();
        let body = &src[start..];
        let end = body.find("\n}").unwrap_or(body.len());
        assert!(
            !body[..end].contains("strip_mcp_registry(&mut overlay)"),
            "`load_readonly` must REMOVE the registry keys, not normalize them to `[]` — an \
             empty array in the overlay would erase the global registry through the merge"
        );
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
            val,
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

    /// A settings file written before this field existed must read back as
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
        let s: Settings = serde_json::from_str(r#"{"schema_version": 29}"#).unwrap();
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

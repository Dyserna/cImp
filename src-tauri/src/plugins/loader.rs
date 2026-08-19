//! V38 Phase A — **discovery**: which manifest files exist, which of them load,
//! and what to say about the ones that do not.
//!
//! Plugins live in the global `<exe-dir>/plugins/` directory and **only** there
//! (decision 8) — the same exe-adjacent external-file arrangement `themes/` and
//! `palettes/` already use, resolved the same way (`theming::themes_dir`). There
//! is deliberately no per-project plugins folder: a project directory is
//! attacker-writable by anything running in it, and a plugin is an argv template
//! plus a grant request.
//!
//! **Rejection is loud, never a skip.** A file that fails to parse or validate
//! becomes a [`PluginError`] carried in the very same [`PluginSet`] the good
//! plugins are in — so Phase B's settings pane renders it as an error state
//! where the user would go to fix it — plus a `plugin` Events row. A silently
//! skipped plugin is a user staring at a settings list wondering why their tool
//! is not there.
//!
//! **Identity is (name, version), both mandatory** (decision 9):
//! * exact duplicate found twice ⇒ **neither** loads, and the conflict names
//!   BOTH file paths. Not "first wins": which file wins would then depend on
//!   directory-read order, so the same two files could behave differently on two
//!   machines, and the losing file's tools would vanish with no explanation.
//! * same name, different version ⇒ both load; tools are namespaced
//!   `name@version/tool-id`, so coexisting versions never clash.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use super::manifest::{self, PluginManifest, Provenance, ValidationError};

/// `<exe-dir>/plugins`. `None` if `current_exe()` has no usable parent — the
/// same fallback shape as [`theming`](crate::theming), where the registry is
/// then simply empty rather than an error.
pub fn plugins_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("plugins"))
}

/// One plugin that loaded: its validated manifest plus the facts only the
/// loader knows — where it came from on disk, and its [`Provenance`].
#[derive(Clone, Debug, Serialize)]
pub struct LoadedPlugin {
    /// Absolute path of the manifest file (display string). Shown in settings
    /// and in conflict rows: the file is the thing a user edits.
    pub path: String,
    /// **Stamped here, never read from the file** — see
    /// [`manifest::Provenance`].
    pub provenance: Provenance,
    /// `name@version`.
    pub key: String,
    pub manifest: PluginManifest,
}

impl LoadedPlugin {
    /// The globally unique id of one of this plugin's tools,
    /// `name@version/tool-id`. The namespace is what lets two versions of the
    /// same plugin coexist without either shadowing the other.
    ///
    /// Phase A declares the contract; Phase B's registry is its first caller,
    /// so this is exercised by tests only until then.
    #[allow(dead_code)]
    pub fn tool_key(&self, tool_id: &str) -> String {
        format!("{}/{}", self.key, tool_id)
    }
}

/// Why a manifest file did not load — the machine-readable half, so the
/// settings pane and the Events row can style the case rather than parse prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginErrorKind {
    /// The file could not be read.
    Io,
    /// It did not parse, or a field failed validation.
    Invalid,
    /// Two files claim the same (name, version) — **neither** loaded.
    Conflict,
}

/// One rejected manifest, kept in the loaded state so the failure is visible
/// where it happened (Events) *and* where it gets fixed (settings).
#[derive(Clone, Debug, Serialize)]
pub struct PluginError {
    pub kind: PluginErrorKind,
    /// Every file this error is about. One for a parse/validation failure;
    /// **two or more** for a [`PluginErrorKind::Conflict`], which is the whole
    /// point — "a duplicate exists" is useless without saying which files.
    pub paths: Vec<String>,
    /// `name@version` when the file got far enough to have an identity.
    pub key: Option<String>,
    /// The human-readable reason, as rendered in settings and in the row.
    pub reason: String,
}

impl PluginError {
    /// The file name(s), for the Events row's `source` column — the identifier
    /// short enough for a table cell, where the full paths go in the detail.
    pub fn file_names(&self) -> String {
        self.paths
            .iter()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

/// The result of one scan: what loaded, and what did not. Both halves, always —
/// a caller that only receives the successes cannot render the failures, which
/// is how "rejected loudly" degrades into "skipped quietly".
#[derive(Clone, Debug, Default, Serialize)]
pub struct PluginSet {
    pub plugins: Vec<LoadedPlugin>,
    pub errors: Vec<PluginError>,
    /// Absolute path of the directory scanned, for the settings pane's "no
    /// plugins yet — drop them here" affordance. Empty when `current_exe()`
    /// yielded no usable parent.
    pub dir: String,
    /// Epoch millis of the scan.
    pub scanned_at_ms: u64,
    /// How long the scan took, for the summary Events row.
    pub scan_ms: u64,
}

/// Read + validate one manifest file. Errors are values, not logs: every path
/// out of here is something the caller puts in front of the user.
fn load_file(path: &Path, provenance: Provenance) -> Result<LoadedPlugin, PluginError> {
    let display = path.display().to_string();
    // Size BEFORE the read: `plugins/` is a directory anything running as the
    // user can write into, and the startup scan reads every `.json` in it — so
    // one enormous file would otherwise be an out-of-memory launch rather than
    // a rejected plugin. `manifest::parse` re-checks the text it is handed
    // (that is the contract, and it covers Phase E's embedded manifests); this
    // is the resource guard. Both name the same `MAX_MANIFEST_BYTES`.
    //
    // A metadata failure is deliberately NOT a verdict — fall through and let
    // the read below report the real I/O error.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > manifest::MAX_MANIFEST_BYTES {
            return Err(PluginError {
                kind: PluginErrorKind::Invalid,
                paths: vec![display],
                key: None,
                reason: ValidationError::Size { bytes: meta.len() }.to_string(),
            });
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| PluginError {
        kind: PluginErrorKind::Io,
        paths: vec![display.clone()],
        key: None,
        reason: format!("could not read the manifest: {e}"),
    })?;
    match manifest::parse(&text, provenance) {
        Ok(m) => Ok(LoadedPlugin {
            path: display,
            provenance,
            key: m.key(),
            manifest: m,
        }),
        // The identity travels WITH the failure ([`manifest::ParseFailure`]): a
        // file that failed validation has already been deserialized once, so
        // re-parsing its text to fish the name back out would be a second full
        // parse of input we just refused. A file that never parsed carries
        // `None` and the settings pane shows its file name — the honest answer
        // rather than a guessed identity in the audit trail.
        Err(f) => Err(PluginError {
            kind: PluginErrorKind::Invalid,
            paths: vec![display],
            key: f.key,
            reason: f.error.to_string(),
        }),
    }
}

/// Scan one directory for `*.json` manifests (non-recursive) and apply the
/// identity rules. Pure: no Events, no global state — so the whole of the
/// discovery contract is testable against a temp directory.
pub fn scan_dir(dir: &Path, provenance: Provenance) -> PluginSet {
    let started = std::time::Instant::now();
    let mut set = PluginSet {
        dir: dir.display().to_string(),
        scanned_at_ms: crate::activity::now_ms(),
        ..PluginSet::default()
    };

    // Sorted by path so a scan is deterministic: the order two conflicting
    // files are reported in must not depend on the filesystem's whim.
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            })
            .collect(),
        // A missing plugins/ directory is the normal state of a fresh install,
        // not an error to put in front of anyone.
        Err(_) => Vec::new(),
    };
    files.sort();

    let mut ok: Vec<LoadedPlugin> = Vec::new();
    for path in &files {
        match load_file(path, provenance) {
            Ok(p) => ok.push(p),
            Err(e) => set.errors.push(e),
        }
    }

    // Identity: group by `name@version`. A group of one loads; a group of more
    // than one loads NOTHING and mints a conflict naming every offending file.
    let mut groups: BTreeMap<String, Vec<LoadedPlugin>> = BTreeMap::new();
    for p in ok {
        groups.entry(p.key.clone()).or_default().push(p);
    }
    for (key, mut group) in groups {
        if group.len() == 1 {
            set.plugins.push(group.remove(0));
            continue;
        }
        let paths: Vec<String> = group.iter().map(|p| p.path.clone()).collect();
        set.errors.push(PluginError {
            kind: PluginErrorKind::Conflict,
            reason: format!(
                "{} files declare the plugin `{key}`, so NEITHER was loaded — two definitions \
                 with one identity cannot be told apart, and picking one by directory order \
                 would make the same pair behave differently on another machine. Delete or \
                 re-version one of: {}",
                paths.len(),
                paths.join(", ")
            ),
            paths,
            key: Some(key),
        });
    }

    set.plugins.sort_by(|a, b| a.key.cmp(&b.key));
    set.errors.sort_by(|a, b| a.paths.cmp(&b.paths));
    set.scan_ms = started.elapsed().as_millis() as u64;
    set
}

/// The app-managed plugin state: one [`PluginSet`], replaced **atomically** by
/// a rescan.
///
/// Atomic replacement rather than in-place mutation is the contract Phase B's
/// registry depends on: a reader holding the `Arc` it got keeps a coherent set
/// for as long as it needs one, and never observes a half-rescanned world where
/// some plugins are from before the scan and some from after.
pub struct PluginStore {
    set: RwLock<Arc<PluginSet>>,
}

impl PluginStore {
    /// An empty store — nothing scanned yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            set: RwLock::new(Arc::new(PluginSet::default())),
        })
    }

    /// The current set. Cheap (one `Arc` clone) and always coherent.
    pub fn snapshot(&self) -> Arc<PluginSet> {
        self.set
            .read()
            .map(|g| g.clone())
            // A poisoned lock must not take the plugin list down with it: an
            // empty set reads as "nothing loaded", which a rescan repairs.
            .unwrap_or_else(|_| Arc::new(PluginSet::default()))
    }

    /// Swap in a new set. The one mutation point.
    pub fn replace(&self, next: PluginSet) -> Arc<PluginSet> {
        let next = Arc::new(next);
        if let Ok(mut g) = self.set.write() {
            *g = next.clone();
        }
        next
    }

    /// Scan `<exe-dir>/plugins/`, mint the `plugin` Events rows for what it
    /// found, and swap the result in. The startup scan and the manual Rescan
    /// are the same call — one code path, so the two can never disagree about
    /// what "loaded" means.
    pub fn rescan(&self) -> Arc<PluginSet> {
        let set = match plugins_dir() {
            Some(dir) => scan_dir(&dir, Provenance::User),
            None => PluginSet {
                scanned_at_ms: crate::activity::now_ms(),
                ..PluginSet::default()
            },
        };
        super::events::record_scan(&set);
        self.replace(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh temp directory for one test. Named after the test so a failure
    /// leaves an inspectable directory rather than a shared one.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cimp-plugins-{tag}-{}-{}",
            std::process::id(),
            crate::activity::now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp plugins dir");
        dir
    }

    fn manifest_text(name: &str, version: &str, tool: &str) -> String {
        format!(
            r#"{{
              "manifest_version": 1,
              "name": "{name}",
              "version": "{version}",
              "categories": [{{ "id": "c", "label": "C", "tools": ["{tool}"] }}],
              "tools": [{{ "id": "{tool}", "label": "T", "kind": "command" }}]
            }}"#
        )
    }

    fn write(dir: &Path, file: &str, body: &str) -> PathBuf {
        let p = dir.join(file);
        fs::write(&p, body).expect("write manifest");
        p
    }

    #[test]
    fn a_directory_of_manifests_loads() {
        let dir = temp_dir("scan");
        write(&dir, "a.json", &manifest_text("alpha", "1.0.0", "one"));
        write(&dir, "b.json", &manifest_text("beta", "0.2.0", "two"));
        // Non-JSON neighbours are not manifests and must not become errors.
        write(&dir, "notes.txt", "hello");

        let set = scan_dir(&dir, Provenance::User);
        assert_eq!(set.errors.len(), 0, "{:?}", set.errors);
        let keys: Vec<&str> = set.plugins.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["alpha@1.0.0", "beta@0.2.0"]);
        assert!(set.plugins.iter().all(|p| p.provenance == Provenance::User));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A missing `plugins/` directory is a fresh install, not a fault.
    #[test]
    fn a_missing_directory_scans_empty_without_erroring() {
        let dir = std::env::temp_dir().join("cimp-plugins-does-not-exist-xyz");
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.plugins.is_empty());
        assert!(set.errors.is_empty());
    }

    /// Decision 9's exact-duplicate rule, and the reason it names both files.
    #[test]
    fn an_exact_duplicate_pair_loads_neither_and_names_both_paths() {
        let dir = temp_dir("dup");
        let a = write(&dir, "a.json", &manifest_text("acme", "1.0.0", "t"));
        let b = write(&dir, "b.json", &manifest_text("acme", "1.0.0", "t"));

        let set = scan_dir(&dir, Provenance::User);
        assert!(
            set.plugins.is_empty(),
            "an ambiguous identity must load NEITHER definition"
        );
        assert_eq!(set.errors.len(), 1);
        let e = &set.errors[0];
        assert_eq!(e.kind, PluginErrorKind::Conflict);
        assert_eq!(e.key.as_deref(), Some("acme@1.0.0"));
        for p in [&a, &b] {
            assert!(
                e.paths.contains(&p.display().to_string()),
                "the conflict must name {p:?}; it named {:?}",
                e.paths
            );
            assert!(e.reason.contains(&p.display().to_string()));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The other half of decision 9: versions coexist, namespaced.
    #[test]
    fn two_versions_of_one_plugin_both_load_with_namespaced_tool_ids() {
        let dir = temp_dir("versions");
        write(&dir, "old.json", &manifest_text("acme", "1.0.0", "scan"));
        write(&dir, "new.json", &manifest_text("acme", "2.0.0", "scan"));

        let set = scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        let keys: Vec<&str> = set.plugins.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["acme@1.0.0", "acme@2.0.0"]);
        // The same tool id in both — distinct only because of the namespace.
        let ids: Vec<String> = set.plugins.iter().map(|p| p.tool_key("scan")).collect();
        assert_eq!(ids, vec!["acme@1.0.0/scan", "acme@2.0.0/scan"]);
        assert_ne!(ids[0], ids[1]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Loud, not fatal: one bad file must not cost the user their good ones,
    /// and must not vanish either.
    #[test]
    fn an_invalid_file_becomes_an_error_entry_and_its_siblings_still_load() {
        let dir = temp_dir("mixed");
        write(&dir, "good.json", &manifest_text("good", "1.0.0", "t"));
        write(&dir, "broken.json", "{ this is not json");
        let versioned = write(
            &dir,
            "future.json",
            &manifest_text("future", "1.0.0", "t").replace("\"manifest_version\": 1", "\"manifest_version\": 9"),
        );

        let set = scan_dir(&dir, Provenance::User);
        assert_eq!(
            set.plugins.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            vec!["good@1.0.0"]
        );
        assert_eq!(set.errors.len(), 2, "{:?}", set.errors);
        assert!(set.errors.iter().all(|e| e.kind == PluginErrorKind::Invalid));
        // A validation failure that got past parsing still carries an identity,
        // so settings can label the row with the plugin rather than a file name.
        let future = set
            .errors
            .iter()
            .find(|e| e.paths[0] == versioned.display().to_string())
            .expect("the version error");
        assert_eq!(future.key.as_deref(), Some("future@1.0.0"));
        assert!(future.reason.contains('9'), "{}", future.reason);
        // …while an unparseable file honestly claims none.
        let broken = set
            .errors
            .iter()
            .find(|e| e.paths[0].ends_with("broken.json"))
            .expect("the syntax error");
        assert!(broken.key.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Provenance is the loader's stamp and the security gates key off it — so
    /// a scanned file can never come back marked built in.
    #[test]
    fn a_scanned_plugin_is_always_user_provenance() {
        let dir = temp_dir("prov");
        write(
            &dir,
            "a.json",
            &manifest_text("acme", "1.0.0", "t").replace(
                "\"manifest_version\": 1,",
                "\"manifest_version\": 1, \"builtin\": true,",
            ),
        );
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.plugins.is_empty());
        assert_eq!(set.errors.len(), 1);
        assert!(
            set.errors[0].reason.contains("stamped by cImp"),
            "{}",
            set.errors[0].reason
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reserved_prefix_is_refused_on_the_scanned_path() {
        let dir = temp_dir("reserved");
        write(&dir, "a.json", &manifest_text("cimp-evil", "1.0.0", "t"));
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.plugins.is_empty());
        assert_eq!(set.errors.len(), 1);
        assert!(set.errors[0].reason.contains("reserved"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The resource half of the size cap: an oversized file is refused from its
    /// directory entry, before a byte of it is read into memory. `plugins/` is
    /// writable by anything running as the user and the scan reads every
    /// `.json` in it, so "read it, then decide" is an out-of-memory launch.
    #[test]
    fn an_oversized_manifest_is_refused_without_being_read() {
        let dir = temp_dir("huge");
        let body = format!(
            "{}{}",
            manifest_text("huge", "1.0.0", "t"),
            " ".repeat(manifest::MAX_MANIFEST_BYTES as usize)
        );
        write(&dir, "huge.json", &body);
        // A good neighbour still loads: one bad file never costs the others.
        write(&dir, "ok.json", &manifest_text("fine", "1.0.0", "t"));

        let set = scan_dir(&dir, Provenance::User);
        assert_eq!(
            set.plugins.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            vec!["fine@1.0.0"]
        );
        assert_eq!(set.errors.len(), 1, "{:?}", set.errors);
        assert_eq!(set.errors[0].kind, PluginErrorKind::Invalid);
        assert!(
            set.errors[0].reason.contains(&body.len().to_string()),
            "the refusal must name the size: {}",
            set.errors[0].reason
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The store swaps whole sets. A reader holding the previous `Arc` keeps
    /// seeing a coherent world — that is what makes a rescan safe to run while
    /// the registry is being read.
    #[test]
    fn a_rescan_replaces_the_set_atomically() {
        let dir = temp_dir("store");
        write(&dir, "a.json", &manifest_text("alpha", "1.0.0", "t"));

        let store = PluginStore::new();
        assert!(store.snapshot().plugins.is_empty());

        let first = store.replace(scan_dir(&dir, Provenance::User));
        assert_eq!(first.plugins.len(), 1);

        write(&dir, "b.json", &manifest_text("beta", "1.0.0", "t"));
        let second = store.replace(scan_dir(&dir, Provenance::User));
        assert_eq!(second.plugins.len(), 2);
        // The handle taken before the swap is unchanged — no reader can observe
        // a half-replaced set.
        assert_eq!(first.plugins.len(), 1);
        assert_eq!(store.snapshot().plugins.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}

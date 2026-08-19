//! V25 Phase A — the **language census**.
//!
//! A cheap, ignore-respecting scan of the project root that answers "which
//! languages/markers are present" so the runner (Phase C) can gate a quality
//! tool out of a project it doesn't apply to — no PMD chip in a Rust repo. It
//! collects lowercase file extensions and hits on a fixed [`MARKERS`] list.
//!
//! The walk is **bounded** ([`MAX_ENTRIES`] / [`MAX_WALK`]): a truncated census
//! only ever *hides* a tool (fewer extensions/markers seen), never invents one,
//! and 20k files is far past where every mainstream language has shown up.
//! Results are cached per root with a short TTL ([`CACHE_TTL`]) so a tab-open
//! and the scan it triggers share one walk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

/// Max filesystem entries the census walk visits before stopping. Truncation is
/// safe — see the module docs.
const MAX_ENTRIES: usize = 20_000;

/// Wall-clock bound on the census walk. Same truncation contract as
/// [`MAX_ENTRIES`], whichever trips first.
const MAX_WALK: Duration = Duration::from_secs(2);

/// How long a cached census stays fresh: long enough that a tab-open and the
/// scan it triggers reuse one walk, short enough that adding a `go.mod` shows up
/// on the next scan.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// The fixed marker tokens the census recognizes. Each is a stable string an
/// adapter's [`super::adapters::Applicability::markers`] references verbatim;
/// [`marker_for`] maps a concrete filename to its token. Glob-shaped markers
/// (`*.sln`, `*.csproj`) and the eslint-config families collapse to one token.
/// Only the lockstep test reads it — production code goes through [`marker_for`].
#[cfg_attr(not(test), allow(dead_code))]
pub const MARKERS: &[&str] = &[
    "go.mod",
    "Cargo.toml",
    "package.json",
    "*.sln",
    "*.csproj",
    "eslint.config",
    ".eslintrc",
    // V38 Phase F: the two build files the plugin contract's own worked example
    // names. `applicability` became a real gate for `check`-kind tools in this
    // phase, and the promise § 8 was corrected to make — "`pom.xml` → maven,
    // `build.gradle` → gradle" — is only true if the census can SEE those files.
    // A closed vocabulary is the point (a manifest names a token cImp owns, not
    // a glob it invents), so making the example expressible means adding to it.
    "pom.xml",
    "build.gradle",
];

/// The result of a census walk: the lowercase extensions and the [`MARKERS`]
/// tokens seen. Fields are private so the invariant "markers ⊆ [`MARKERS`]" and
/// "extensions are lowercase, dot-less" holds by construction; read via the
/// accessors. Phase C serializes [`extensions`](Self::extensions) /
/// [`markers`](Self::markers) into the `audit_snapshot` census block.
#[derive(Clone, Debug, Default)]
pub struct Census {
    extensions: HashSet<String>,
    markers: HashSet<&'static str>,
    /// V38 F-1: for each marker seen, the **shallowest** directory it was seen
    /// in, relative to the walked root and `/`-separated (`""` = the root
    /// itself).
    ///
    /// The census matches recursively, so `src-tauri/Cargo.toml` admits a tool
    /// gated on `Cargo.toml` — but the rendered check then ran at the project
    /// ROOT, where `cargo build` exits 101 in 140 ms because there is no
    /// manifest there. The census knew where the marker was and threw the
    /// location away; this field is that location, kept.
    ///
    /// Shallowest, and one per marker: a check is one command in one directory,
    /// so the census must answer with one directory. Ties (two hits at the same
    /// depth) break lexicographically, so the answer is deterministic across
    /// walks whose entry order the filesystem chose.
    marker_dirs: HashMap<&'static str, String>,
}

impl Census {
    /// Rehydrate a census from the serialized census block a snapshot carries.
    ///
    /// V38 Phase D: `effective_roster` answers "would this tool run?" from the
    /// census the LAST walk stored, without walking again. That is the same
    /// applicability question `plan_scan` asks, so it must be asked OF A
    /// `Census` rather than re-implemented against two string vectors.
    ///
    /// A marker string that is not one of [`MARKERS`] is dropped rather than
    /// interned: the invariant `markers ⊆ MARKERS` is what makes the private
    /// representation safe, and a block that has been through a snapshot is not
    /// a reason to weaken it.
    /// `marker_dirs` is deliberately EMPTY here: the serialized census block
    /// carries names, not locations, and a rehydrated census answers the
    /// applicability question (`admits`) rather than the "where does this run"
    /// one. A consumer that gets `None` from [`Self::marker_dir`] falls back to
    /// the root, which is the pre-F-1 behaviour — the safe direction for a
    /// census that was serialized before locations existed.
    pub fn from_block(extensions: &[String], markers: &[String]) -> Census {
        Census {
            extensions: extensions.iter().cloned().collect(),
            markers: markers
                .iter()
                .filter_map(|m| MARKERS.iter().find(|known| *known == m).copied())
                .collect(),
            marker_dirs: HashMap::new(),
        }
    }

    /// Whether a file with this (lowercase, dot-less) extension was seen.
    pub fn has_extension(&self, ext: &str) -> bool {
        self.extensions.contains(ext)
    }

    /// Whether this [`MARKERS`] token was seen.
    pub fn has_marker(&self, marker: &str) -> bool {
        self.markers.contains(marker)
    }

    /// V38 F-1 — the shallowest directory this marker was seen in, relative to
    /// the walked root and `/`-separated. `Some("")` means the root itself;
    /// `None` means the marker was not seen at all, or was seen by a census
    /// that carries no locations (see [`Self::from_block`]).
    ///
    /// The one consumer is [`crate::checks::plugin`], which turns it into a
    /// plugin check's `cwd` so a check admitted by a nested marker runs where
    /// that marker is. Deliberately NOT part of [`Self::admits`]: whether a
    /// tool applies and where it should run are two questions, and folding them
    /// together would make a gate that passes at the root and fails one
    /// directory down.
    pub fn marker_dir(&self, marker: &str) -> Option<&str> {
        self.marker_dirs.get(marker).map(String::as_str)
    }

    /// Record a marker hit's directory, keeping the shallowest one and breaking
    /// depth ties lexicographically.
    ///
    /// Both halves matter. Shallowest is the right answer for a monorepo whose
    /// root manifest is the workspace (`Cargo.toml` at the root wins over
    /// `crates/foo/Cargo.toml`), and the lexicographic tie-break is what makes
    /// the value independent of the order `ignore`'s parallel-ready walk
    /// happened to yield entries in — without it the same tree could produce
    /// two different `cwd`s on two runs.
    fn note_marker_dir(&mut self, marker: &'static str, dir: String) {
        match self.marker_dirs.get(marker) {
            Some(current) if !dir_is_better(&dir, current) => {}
            _ => {
                self.marker_dirs.insert(marker, dir);
            }
        }
    }

    /// **The** applicability rule: no gate = always applicable, else ANY listed
    /// extension OR ANY listed marker.
    ///
    /// V38 Phase F extracted it here from `audit::runnable::RunnableAudit`,
    /// which had been its only reader, because it stopped being an audit-only
    /// question: `check`-kind plugin tools are gated by the same manifest field
    /// through `checks::plugin::effective_checks`, and a second copy of the rule
    /// would let a tool be applicable under one umbrella and not under the
    /// pipeline next door. One rule, three populations (built-in audit tools,
    /// plugin audit tools, plugin checks) — a plugin tool must not be gateable
    /// differently from a built-in one.
    ///
    /// The vocabulary is closed on purpose (see [`MARKERS`]): a manifest names a
    /// token cImp owns rather than a glob it invents, so an author cannot make
    /// the census walk look for something it was never taught to see.
    pub fn admits(&self, a: &crate::plugins::manifest::Applicability) -> bool {
        if a.extensions.is_empty() && a.markers.is_empty() {
            return true;
        }
        a.extensions.iter().any(|e| self.has_extension(e))
            || a.markers.iter().any(|m| self.has_marker(m))
    }

    /// The seen extensions, sorted — the chip-visibility payload (Phase C).
    pub fn extensions(&self) -> Vec<String> {
        let mut v: Vec<String> = self.extensions.iter().cloned().collect();
        v.sort();
        v
    }

    /// The seen marker tokens, sorted — the chip-visibility payload (Phase C).
    pub fn markers(&self) -> Vec<String> {
        let mut v: Vec<String> = self.markers.iter().map(|s| (*s).to_string()).collect();
        v.sort();
        v
    }
}

/// Whether `candidate` should replace `current` as a marker's recorded
/// directory: shallower wins, and at equal depth the lexicographically smaller
/// one does. Shared by [`Census::note_marker_dir`] and by
/// [`crate::checks::plugin`], which applies the SAME ordering when a manifest
/// gates on several markers at once — one rule, so "shallowest, ties
/// lexicographic" cannot mean two things one call apart.
pub(crate) fn dir_is_better(candidate: &str, current: &str) -> bool {
    (dir_depth(candidate), candidate) < (dir_depth(current), current)
}

/// Path depth of a `/`-separated relative census directory; the root (`""`) is
/// 0. Not `split('/').count()`, which counts `""` as one segment.
fn dir_depth(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.split('/').count()
    }
}

/// Map a filename to its [`MARKERS`] token, `None` if it isn't a marker. Case-
/// insensitive; the glob and eslint-config families collapse to one token each.
fn marker_for(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "go.mod" => Some("go.mod"),
        "cargo.toml" => Some("Cargo.toml"),
        "package.json" => Some("package.json"),
        "pom.xml" => Some("pom.xml"),
        // Groovy and Kotlin DSL collapse to ONE token, the `*.sln` family's
        // rule: a plugin author gating on "this is a Gradle project" must not
        // have to know which of the two spellings the project chose.
        "build.gradle" | "build.gradle.kts" => Some("build.gradle"),
        _ => {
            if lower.ends_with(".sln") {
                Some("*.sln")
            } else if lower.ends_with(".csproj") {
                Some("*.csproj")
            } else if is_eslint_flat_config(&lower) {
                Some("eslint.config")
            } else if lower == ".eslintrc" || lower.starts_with(".eslintrc.") {
                Some(".eslintrc")
            } else {
                None
            }
        }
    }
}

/// Whether `name` (already lowercased) is a flat ESLint config
/// (`eslint.config.{js,mjs,cjs,ts}`).
fn is_eslint_flat_config(name: &str) -> bool {
    name.starts_with("eslint.config.")
        && (name.ends_with(".js")
            || name.ends_with(".mjs")
            || name.ends_with(".cjs")
            || name.ends_with(".ts"))
}

/// A census for `root`, reusing a recent cached walk (within [`CACHE_TTL`]) or
/// taking a fresh one. The entry point the runner/tab use so a tab-open and the
/// scan it triggers don't double-walk. The cache is TTL-only — there is no
/// explicit invalidation; a stale entry ages out in ≤ 60 s.
pub fn cached(root: &Path) -> Census {
    let cache = census_cache();
    let key = root.to_path_buf();
    {
        let map = cache.lock().unwrap();
        if let Some((at, census)) = map.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return census.clone();
            }
        }
    }
    let fresh = take(root);
    cache
        .lock()
        .unwrap()
        .insert(key, (Instant::now(), fresh.clone()));
    fresh
}

/// Per-root census cache: `(taken_at, census)` keyed by absolute root. TTL-only,
/// so it never needs draining — a superseded entry is simply overwritten.
fn census_cache() -> &'static Mutex<HashMap<PathBuf, (Instant, Census)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, Census)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take a fresh (uncached) census of `root` with the production bounds. The
/// bounded core is [`take_bounded`], kept separate so tests can drive a tiny
/// bound without a 20k-file fixture.
pub fn take(root: &Path) -> Census {
    take_bounded(root, MAX_ENTRIES, MAX_WALK)
}

/// The census walk. Honors `.gitignore` like the graph indexer and skips hidden
/// *directories* (`.git`, `.cargo`, …) — but NOT hidden files, so a root
/// `.eslintrc` is still classified. Stops at `max_entries` visited entries or
/// `max_walk` elapsed, whichever first; a truncated result only hides tools.
fn take_bounded(root: &Path, max_entries: usize, max_walk: Duration) -> Census {
    let mut census = Census::default();
    let started = Instant::now();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|e| {
            // Skip hidden dirs (don't descend into `.git`/dot-dirs) while still
            // yielding hidden files like `.eslintrc`. Never filter the root.
            if e.depth() == 0 {
                return true;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let hidden = e
                .file_name()
                .to_str()
                .map(|n| n.starts_with('.'))
                .unwrap_or(false);
            !(is_dir && hidden)
        })
        .build();

    let mut seen = 0usize;
    for entry in walker {
        seen += 1;
        if seen > max_entries || started.elapsed() >= max_walk {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        // Dir entries only advance the bound counter; extensions/markers are a
        // file property.
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            census.extensions.insert(ext.to_ascii_lowercase());
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(m) = marker_for(name) {
                census.markers.insert(m);
                // …and WHERE it was seen (F-1). Derived from the entry's own
                // path against the root the walk started from, so it needs no
                // second traversal and costs one `strip_prefix` per marker hit
                // — of which a project has a handful.
                census.note_marker_dir(m, relative_dir(root, path));
            }
        }
    }
    census
}

/// The directory of `path`'s parent relative to `root`, `/`-separated, `""` for
/// a file directly in the root.
///
/// `/`-separated on every platform because the value becomes a `CheckDef::cwd`,
/// which is confined and joined by the same code on Windows and Unix, and
/// because a backslash in a stored relative path is a needless second spelling
/// of the same directory. A path that is somehow not under `root` yields `""`
/// (the root) — the pre-F-1 answer, and the only safe one for a value that is
/// about to be confined beneath that root.
fn relative_dir(root: &Path, path: &Path) -> String {
    path.parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

#[cfg(test)]
impl Census {
    /// Test-only: build a census from explicit parts, so `adapters` applicability
    /// tests can pin an exact project shape without a tempdir walk.
    pub(crate) fn from_parts(extensions: &[&str], markers: &[&'static str]) -> Census {
        Census {
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            markers: markers.iter().copied().collect(),
            marker_dirs: HashMap::new(),
        }
    }

    /// Test-only: the same, plus each marker's recorded directory — so a check
    /// seam test can pin an exact project SHAPE *and* an exact project LAYOUT
    /// without laying out a tree on disk.
    pub(crate) fn from_parts_with_dirs(
        extensions: &[&str],
        markers: &[(&'static str, &str)],
    ) -> Census {
        Census {
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            markers: markers.iter().map(|(m, _)| *m).collect(),
            marker_dirs: markers
                .iter()
                .map(|(m, d)| (*m, (*d).to_string()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tempdir project fixture, cleaned on drop — the `checks::detect` pattern.
    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("audit-census-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }
        fn file(&self, rel: &str, contents: &str) -> &Self {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, contents).unwrap();
            self
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn every_marker_token_is_declared() {
        // Every token `marker_for` can emit must be a member of MARKERS — the
        // published universe Phase C serializes and adapters reference.
        for name in [
            "go.mod",
            "Cargo.toml",
            "package.json",
            "App.sln",
            "x.csproj",
            "eslint.config.js",
            ".eslintrc",
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
        ] {
            let tok = marker_for(name).unwrap_or_else(|| panic!("{name} should classify"));
            assert!(MARKERS.contains(&tok), "token {tok:?} missing from MARKERS");
        }
        assert_eq!(
            marker_for("build.gradle.kts"),
            Some("build.gradle"),
            "the Kotlin DSL is the same project shape as the Groovy one"
        );
        assert_eq!(
            MARKERS.len(),
            9,
            "MARKERS and marker_for must stay in lockstep"
        );
    }

    #[test]
    fn extensions_collected_lowercase() {
        let fx = Fixture::new();
        fx.file("src/main.RS", "")
            .file("web/App.TSX", "")
            .file("x.go", "");
        let c = take(&fx.root);
        assert!(c.has_extension("rs"), "uppercase RS folded to rs");
        assert!(c.has_extension("tsx"));
        assert!(c.has_extension("go"));
        // The accessor is sorted and dot-less.
        assert_eq!(c.extensions(), vec!["go", "rs", "tsx"]);
    }

    #[test]
    fn markers_detected_including_globs_and_eslintrc_variants() {
        let fx = Fixture::new();
        fx.file("Cargo.toml", "")
            .file("go.mod", "")
            .file("package.json", "{}")
            .file("App.sln", "")
            .file("Web.csproj", "")
            .file("eslint.config.mjs", "")
            .file(".eslintrc.json", "");
        let c = take(&fx.root);
        assert!(c.has_marker("Cargo.toml"));
        assert!(c.has_marker("go.mod"));
        assert!(c.has_marker("package.json"));
        assert!(c.has_marker("*.sln"), "*.sln glob marker from App.sln");
        assert!(
            c.has_marker("*.csproj"),
            "*.csproj glob marker from Web.csproj"
        );
        assert!(c.has_marker("eslint.config"), "flat eslint config token");
        assert!(c.has_marker(".eslintrc"), ".eslintrc* legacy token");
        assert_eq!(
            c.markers(),
            vec![
                "*.csproj",
                "*.sln",
                ".eslintrc",
                "Cargo.toml",
                "eslint.config",
                "go.mod",
                "package.json"
            ]
        );
    }

    #[test]
    fn eslintrc_bare_and_flat_config_extensions() {
        let fx = Fixture::new();
        fx.file(".eslintrc", "").file("eslint.config.ts", "");
        let c = take(&fx.root);
        assert!(c.has_marker(".eslintrc"), "bare .eslintrc is a marker");
        assert!(c.has_marker("eslint.config"));
        // A non-flat eslint.config.json is NOT a flat-config marker.
        let fx2 = Fixture::new();
        fx2.file("eslint.config.json", "");
        assert!(!take(&fx2.root).has_marker("eslint.config"));
    }

    #[test]
    fn gitignored_files_excluded() {
        let fx = Fixture::new();
        // A real git repo so `.gitignore` is honored by the walker.
        fx.file(".gitignore", "ignored/\n");
        fx.file("kept.rs", "");
        fx.file("ignored/secret.go", "");
        // Init a git repo (the ignore crate needs a repo boundary for gitignore).
        let _ = std::process::Command::new("git")
            .arg("init")
            .current_dir(&fx.root)
            .output();
        let c = take(&fx.root);
        assert!(c.has_extension("rs"), "tracked file seen");
        assert!(!c.has_extension("go"), "gitignored subtree excluded");
    }

    #[test]
    fn entry_bound_respected() {
        let fx = Fixture::new();
        // Far more distinct-extension files than the injected bound.
        for i in 0..60 {
            fx.file(&format!("f{i}.ext{i}"), "");
        }
        let c = take_bounded(&fx.root, 5, Duration::from_secs(2));
        assert!(
            c.extensions().len() < 60,
            "the walk stopped at the bound; collected {} of 60",
            c.extensions().len()
        );
    }

    /// V38 F-1 — the census records WHERE each marker was seen, not only that
    /// it was. This repo's own shape is the defect that motivated it: the only
    /// `Cargo.toml` is one directory down, and a check rendered to run at the
    /// root exits 101 before it does any work.
    #[test]
    fn marker_directories_are_recorded_shallowest_first() {
        let fx = Fixture::new();
        fx.file("src-tauri/Cargo.toml", "")
            .file("crates/deep/Cargo.toml", "")
            .file("package.json", "{}");
        let c = take(&fx.root);
        assert_eq!(
            c.marker_dir("Cargo.toml"),
            Some("src-tauri"),
            "the SHALLOWEST hit wins: `src-tauri` is depth 1, `crates/deep` is depth 2 — and \
             this is exactly this repo's own shape"
        );

        // Two hits at the SAME depth: the lexicographic tie-break decides, so
        // two walks of one tree cannot render two different commands.
        let tied = Fixture::new();
        tied.file("src-tauri/Cargo.toml", "").file("cli/Cargo.toml", "");
        assert_eq!(take(&tied.root).marker_dir("Cargo.toml"), Some("cli"));
        // A root hit beats any nested one, whatever the walk order.
        assert_eq!(c.marker_dir("package.json"), Some(""));
        // An unseen marker has no directory at all.
        assert_eq!(c.marker_dir("pom.xml"), None);

        // …and a root `Cargo.toml` added later takes it over: depth beats the
        // tie-break, which is the workspace-root case.
        let fx2 = Fixture::new();
        fx2.file("Cargo.toml", "").file("crates/a/Cargo.toml", "");
        assert_eq!(take(&fx2.root).marker_dir("Cargo.toml"), Some(""));
    }

    /// The ordering rule itself, stated once and shared with the check seam.
    #[test]
    fn shallower_wins_then_lexicographic() {
        assert!(dir_is_better("", "src-tauri"));
        assert!(!dir_is_better("src-tauri", ""));
        assert!(dir_is_better("a", "b"));
        assert!(dir_is_better("zzz", "a/b"), "depth beats the name");
        assert!(!dir_is_better("a/b", "zzz"));
        assert!(!dir_is_better("a", "a"));
    }

    /// A census rehydrated from a serialized block answers the applicability
    /// question but carries no locations — and says so by returning `None`,
    /// which its one consumer reads as "run at the root".
    #[test]
    fn a_rehydrated_census_has_no_marker_directories() {
        let c = Census::from_block(&[], &["Cargo.toml".to_string()]);
        assert!(c.has_marker("Cargo.toml"));
        assert_eq!(c.marker_dir("Cargo.toml"), None);
    }

    #[test]
    fn cached_returns_same_census_within_ttl() {
        let fx = Fixture::new();
        fx.file("a.rs", "");
        let first = cached(&fx.root);
        assert!(first.has_extension("rs"));
        // A second file added after the cache warmed is NOT seen until the TTL
        // expires — proves the second call served the cached walk, not a rewalk.
        fx.file("b.go", "");
        let second = cached(&fx.root);
        assert!(
            !second.has_extension("go"),
            "cached census reused within TTL"
        );
    }
}

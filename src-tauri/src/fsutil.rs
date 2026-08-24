//! Shared path-confinement primitive.
//!
//! Three subsystems resolve a user- or model-supplied path and then refuse it
//! if it escapes a trusted boundary directory:
//!   * `checks` — a check's `cwd`/`report_file` (target may not exist yet: the
//!     run is about to create it);
//!   * `offload::tools::ToolCtx::confine` — native read-only tools, multi-root
//!     with ambiguity detection (target must exist);
//!   * `graph::mcp::confine_to_root` — `graph_snippet` disk reads (target must
//!     exist).
//!
//! Historically each carried its own `canonicalize` + `starts_with` escape
//! check with slightly different edge-case handling, so a future symlink- or
//! `..`-normalization hardening could land in one copy and be missed in the
//! others. This module is the single home for that security-sensitive core.
//! The subsystem-specific pieces (the lexical `..`/absolute pre-guard in
//! `checks`, the multi-root ambiguity errors and root labelling in `offload`,
//! the per-site error messages) stay in their call sites and layer on top.
//!
//! The core operation both variants share: **canonicalize the real, symlink-
//! resolved location of (the existing portion of) a target and verify it stays
//! within the canonicalized boundary.** `canonicalize` is what defeats a
//! symlink escape the lexical check can't see — on every platform it fully
//! resolves symlinks/junctions, and on Windows it also normalizes `..` and
//! yields an extended-length (`\\?\`) prefix, which `starts_with` compares
//! component-wise (both sides canonical, so the prefixes match).

use std::path::{Path, PathBuf};

/// **The per-project cImp data directory**, `<project>/.cimp`.
///
/// Holds the per-folder settings overlay (`config.json`), the code-graph store
/// (`graph.db`), the note (`cimp.note.txt`) and the per-project UI state
/// (`ui_state.json`). One name, here, rather than a private copy per owner.
///
/// It used to be a private `CIMP_DIR_NAME` in `settings::persistence`,
/// re-spelled in `ipc::note` and again in `ipc::ui_state`, each carrying a
/// comment explaining that the copy exists so the module need not depend on
/// the settings internals. That reasoning was right about the coupling and
/// wrong about the remedy: the name is a filesystem-layout fact, not a
/// settings fact, so its home is the path module all of them already depend
/// on. Three literals that must agree and are checked by nothing is how a
/// rename half-lands (V42 review, dropped-at-cap item).
///
/// Deliberately NOT derived from `graph.db_subdir`: the overlay decides where
/// the overlay is read from, so its location cannot depend on a value stored
/// *inside* it. [`find_project_root`] falls back to this same name for the
/// same reason.
pub const CIMP_DIR_NAME: &str = ".cimp";

// ---------------------------------------------------------------------------
// Shared directory-key normalization.
//
// Several seams compare two directory paths for *identity* — "is this the same
// project dir?" — rather than resolving them on disk: the permission hook's
// cwd fallback (`offload::loopback::norm_dir`) and the H1 ambiguity predicate's
// transcript-root key (`graph::service::LiveTabRoot`). They must agree, or one
// of them silently stops matching hand-typed cwds that differ only by case or a
// trailing separator. One implementation, used by both.
// ---------------------------------------------------------------------------

/// A directory string normalized for EQUALITY COMPARISON: separators unified to
/// `/`, trailing separators dropped, and — on Windows, whose filesystem paths
/// are case-insensitive — case-folded. `None` for an empty/whitespace path.
///
/// Purely lexical: no `canonicalize`, no filesystem access, no `..` resolution.
/// That is deliberate — callers key live in-memory maps by it on hot paths and
/// must not touch the disk (nor fail when the dir has since been deleted). The
/// result is a comparison KEY, not a displayable path.
pub fn norm_dir_key(dir: &str) -> Option<String> {
    let s = dir.trim().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    Some(if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    })
}

/// [`norm_dir_key`] for a `Path`, keeping the `PathBuf` type so callers can
/// store the canonical key where a path already lives. A path that normalizes
/// to nothing (empty) is returned unchanged — an unusable key is still better
/// than silently collapsing distinct roots to one.
pub fn norm_dir_key_path(dir: &Path) -> PathBuf {
    match norm_dir_key(&dir.to_string_lossy()) {
        Some(s) => PathBuf::from(s),
        None => dir.to_path_buf(),
    }
}

// ---------------------------------------------------------------------------
// Shared "directories a recursive walk should not descend into" name sets.
//
// Several subsystems do a plain recursive `read_dir` walk and want to avoid
// wasting work (or flooding a channel) inside build-output, vendored-dependency,
// and VCS-metadata trees. Each used to keep its own hand-maintained
// `SKIP_DIRS`/`IGNORE_DIRS` array, and they drifted — one gained
// `.next`/`.svelte-kit`, another `out`/`bin`/`obj` — so a convention added to
// one silently missed the others. They live here now so the sets, and any
// deliberate difference between them, are visible in a single place.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Shared "is this path absolute?" verdict.
//
// Two subsystems refuse an absolute path where a relative one is required —
// `checks::lexically_confined` (a `CheckDef`'s `cwd`/`report_file`) and
// `plugins::manifest` (a plugin tool's `cwd`/`report_file`, and the shape of
// its `extra_grants`, which must be absolute). They must agree, so the
// predicate lives here rather than once per caller.
// ---------------------------------------------------------------------------

/// Whether a path string is absolute in the **platform-agnostic** sense: a
/// POSIX root (`/etc`), a Windows rooted/UNC path (`\\server\share`,
/// `\Windows`), or a drive-letter prefix (`C:\Tools`).
///
/// Deliberately NOT [`Path::is_absolute`], which answers for the *running*
/// platform only: on Windows `"/etc"` is not absolute (it is root-relative to
/// the current drive) and on Linux `"C:\Tools"` is an ordinary relative
/// filename. Both callers treat "absolute" as a REFUSAL condition, so the
/// platform-specific answer refuses one shape and waves the other through —
/// and which one it waves through depends on where the check happens to run:
///
/// * `checks::lexically_confined` guards a `CheckDef`'s `cwd`/`report_file`.
///   The canonical `confine_under_root` re-check catches the escape at spawn
///   time either way, so this is the lexical half reaching the same verdict on
///   both platforms rather than deferring half of them to a later, differently
///   worded failure.
/// * `plugins::manifest` validates a manifest, which is authored once and read
///   on **every** platform: a file refused on Linux must not load on Windows
///   and fail later at run time. One artifact, one verdict.
///
/// A drive-letter form must carry a **separator after the colon**: `C:foo` is
/// *drive-relative* — "foo, relative to whatever the current directory on
/// drive C happens to be" — and `C:` alone names a drive, not a location.
/// Neither is a rooted path, so neither counts as absolute here, and a plugin
/// requesting `C:foo` as an `extra_grant` is refused rather than granted a
/// directory that depends on per-drive process state. (They are still not
/// *relative to the project root* either, but that is the `..`/confinement
/// rule's business; this predicate answers one question.)
pub fn looks_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    let Some(&first) = b.first() else {
        return false;
    };
    // POSIX root, or a Windows UNC / drive-rooted path.
    if first == b'/' || first == b'\\' {
        return true;
    }
    // `X:\` or `X:/` — the separator is required; see the doc comment.
    b.len() >= 3 && first.is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// Directory names a *broad* recursive scan should never descend into: build
/// output, vendored dependencies, and VCS/tool-cache metadata. Used by any full
/// filesystem pass whose cost of a false skip is only wasted scanning, never a
/// missed update — marker auto-detection ([`crate::checks::detect`]) and the
/// offload code-search tool.
///
/// Includes dot-prefixed names so callers that don't otherwise filter dotfiles
/// still skip them; a caller that already skips every dotfile separately (e.g.
/// `checks::detect`, via its leading-dot rule) merely re-skips them here, which
/// is a harmless no-op.
pub const SKIP_DIRS: &[&str] = &[
    // Build output.
    "target",
    "dist",
    "build",
    "out",
    "bin",
    "obj",
    // Dependencies / vendored trees.
    "node_modules",
    "vendor",
    "__pycache__",
    "venv",
    ".venv",
    // VCS metadata and framework/tool caches.
    ".git",
    ".next",
    ".cache",
    ".svelte-kit",
];

/// The graph file-watcher's DELIBERATELY MINIMAL skip set — a small hand-picked
/// subset of [`SKIP_DIRS`], **not** the broad list.
///
/// The watcher drops filesystem events at the source so a `cargo build` /
/// `npm install` can't flood its bounded channel. Here the cost of a false skip
/// is not "wasted scanning" but a **missed re-index**: if a project keeps
/// first-class source under `dist/`, `build/`, `out/`, or `vendor/`, dropping
/// those events would silently stop the code graph from updating. So the watcher
/// only ever skips the three trees that are *never* indexed content — `.git`,
/// `target`, `node_modules` — and lets the gitignore-aware `reindex_paths` pass
/// filter everything else. Kept beside [`SKIP_DIRS`] so the two sets' divergence
/// is an intentional, reviewable decision rather than accidental drift.
///
/// The watcher additionally skips the graph's own store subdir (`.cimp` by
/// default — a runtime setting, so it can't live in this const); see
/// `graph::watcher::all_paths_skippable` for why that filter is load-bearing.
pub const WATCH_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Why a confinement check failed.
#[derive(Debug)]
pub enum ConfineError {
    /// The confinement `boundary` itself could not be canonicalized — usually
    /// it does not exist. Carries the underlying I/O error so callers can build
    /// their own "cannot resolve root" message.
    Boundary(std::io::Error),
    /// The target does not exist on disk. Only returned by [`confine_existing`]
    /// (the must-exist variant); [`confine_creatable`] treats absence as fine.
    NotFound,
    /// The target's real (canonicalized) location escapes the boundary.
    Escaped,
}

/// Canonicalize the boundary once, up front, so every variant resolves the
/// boundary's own symlinks the same way before comparing.
fn canon_boundary(boundary: &Path) -> Result<PathBuf, ConfineError> {
    boundary.canonicalize().map_err(ConfineError::Boundary)
}

/// Confine an **existing** `target` under `boundary`: the target must exist,
/// and its canonical (symlink-resolved) path must lie within the canonical
/// boundary. Returns the canonical target on success.
///
/// Used by read-only readers that only ever touch files already on disk
/// (offload native tools, `graph_snippet` reads). `boundary` and `target` are
/// each canonicalized here; pass `target` already joined onto whatever base the
/// caller wants (e.g. `root.join(rel)`).
pub fn confine_existing(boundary: &Path, target: &Path) -> Result<PathBuf, ConfineError> {
    let root = canon_boundary(boundary)?;
    let canon = target.canonicalize().map_err(|_| ConfineError::NotFound)?;
    if canon.starts_with(&root) {
        Ok(canon)
    } else {
        Err(ConfineError::Escaped)
    }
}

/// Confine a possibly-**not-yet-existing** `target` under `boundary`: walk from
/// the target up to its nearest existing ancestor, canonicalize THAT, and
/// require it to stay within the canonical boundary. A target whose entire path
/// is absent (nothing along it canonicalizes) is vacuously confined — the
/// caller is about to create it, and any lexical `..`/absolute guard has
/// already run separately.
///
/// Returns `Some(canonical_ancestor)` when an existing ancestor was found
/// (almost always, since an absolute target shares the boundary's existing
/// drive/root), else `None`. Callers that only need the confinement verdict can
/// ignore the value. Used for a check's `report_file`/`cwd`, which the run
/// itself may create.
pub fn confine_creatable(boundary: &Path, target: &Path) -> Result<Option<PathBuf>, ConfineError> {
    let root = canon_boundary(boundary)?;
    // Canonicalize the deepest existing part of `target` and confirm it stays
    // under the boundary — catches a symlink escape through an existing parent
    // that the lexical guard can't see, while allowing a leaf that doesn't
    // exist yet.
    match target.ancestors().find_map(|a| a.canonicalize().ok()) {
        Some(existing) if !existing.starts_with(&root) => Err(ConfineError::Escaped),
        Some(existing) => Ok(Some(existing)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Shared project-root resolution (#104).
//
// A working directory is NOT a project root. Every externally supplied `cwd` —
// a Claude hook payload's `cwd`, the OpenCode plugin's, an MCP call's, a
// `run_command` marker directory — arrives from a process cImp does not
// control, and a sub-agent's shell keeps its cwd across calls: one `cd` into
// `src-tauri/src/harness` and every later hook reports THAT as its working
// directory. Taking it as a root attributed activity rows to a directory that
// is not a project and, worse, minted per-project STATE there (a `<db_subdir>`
// holding `graph.db` and the workbench's `shadow.git`) — ten such directories
// under one repo, which is what #104 is.
//
// One resolver, used by every such site, so the answer cannot differ by route.
// ---------------------------------------------------------------------------

/// Which marker ended the [`find_project_root`] walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootMarker {
    /// A `.git` entry — a directory (ordinary clone) or a FILE (a linked
    /// worktree or a submodule, whose `.git` is a one-line gitdir pointer).
    Vcs,
    /// An existing `<db_subdir>` directory — cImp's own per-project state.
    State,
}

/// A resolved project root, plus what #104 wants reported about the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    /// The directory to treat as the project.
    pub root: PathBuf,
    /// Which marker won.
    pub marker: RootMarker,
    /// The `<db_subdir>` directory (the path itself, e.g.
    /// `<root>/src-tauri/src/harness/.cimp`) found strictly BELOW
    /// [`Self::root`] during the walk: state minted under an existing project
    /// root by the defect this resolver closes. Reported (a `warn!` plus an
    /// Activity row at the call site), **never deleted** — it is the user's
    /// data and may hold a graph they want.
    pub stray_state: Option<PathBuf>,
}

/// Whether `dir` carries a VCS root marker.
///
/// `.git` as a **file** counts: that is how git spells a linked worktree and a
/// submodule, and cImp is routinely run from one (this repo's own fix branch
/// lives in a worktree). `exists()` rather than `is_dir()` is the whole point.
fn has_vcs_marker(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// The project root for `start`, walking UP to the nearest ancestor (inclusive)
/// that carries a root marker. `None` when the whole chain carries none — a
/// genuinely new, un-VCS'd folder, for which the caller must fall back to a
/// root it *knows* (the tab's configured directory) or refuse, never mint one.
///
/// **Marker precedence — `.git` is STRONG, `<db_subdir>` is WEAK.**
///
/// * The nearest ancestor with a `.git` (dir or file) wins outright. Nearest,
///   so a nested repo or a submodule beats its outer repo for a cwd inside it.
/// * A `<db_subdir>` directory only wins when **no** `.git` exists anywhere up
///   the chain (a project kept outside version control, indexed by cImp).
/// * When both are present, the `.git` root wins and the lower `<db_subdir>` is
///   returned as [`ProjectRoot::stray_state`].
///
/// The asymmetry is deliberate and is what makes the fix retroactive. A
/// `<db_subdir>` is cImp's own output: after this change it can only exist at a
/// real root, but the directories the defect already minted are indistinguishable
/// from a root by their own presence. Treating them as equal markers would let
/// every stray keep capturing the cwds that created it, forever. `.git` is
/// evidence the *user* placed; `<db_subdir>` is evidence cImp placed, and cImp
/// is the thing that was wrong.
///
/// A caller whose project legitimately sits at a `<db_subdir>`-only directory
/// INSIDE a git repo (a sub-project opened as its own tab) is not served by the
/// walk and must not be: that root is known from the tab's configuration, which
/// every caller here consults FIRST — see `discovery::external_project_root`.
///
/// Purely observational: no directory is created, and nothing is deleted.
pub fn find_project_root(start: &Path, db_subdir: &str) -> Option<ProjectRoot> {
    let sub = match db_subdir.trim() {
        "" => CIMP_DIR_NAME,
        s => s,
    };
    let mut weak: Option<PathBuf> = None;
    for dir in start.ancestors() {
        // An empty component is what `Path::ancestors` ends on for a relative
        // path; probing it would test the PROCESS cwd, which is exactly the
        // "some other directory decides the project" bug in miniature.
        if dir.as_os_str().is_empty() {
            break;
        }
        if has_vcs_marker(dir) {
            // `weak` holds the `<sub>` directory itself; it is a stray only
            // when it does not belong to the root we are about to return.
            let stray = weak.filter(|w| w.parent() != Some(dir));
            return Some(ProjectRoot {
                root: dir.to_path_buf(),
                marker: RootMarker::Vcs,
                stray_state: stray,
            });
        }
        if weak.is_none() && dir.join(sub).is_dir() {
            weak = Some(dir.join(sub));
        }
    }
    weak.map(|state| ProjectRoot {
        root: state.parent().unwrap_or(&state).to_path_buf(),
        marker: RootMarker::State,
        stray_state: None,
    })
}

// ---------------------------------------------------------------------------
// Windows verbatim (`\\?\`) prefix normalization.
//
// `canonicalize` returns the extended-length form on Windows, so the SAME
// directory reaches the activity store as `\\?\P:\proj` from any path that was
// canonicalized and as `P:\proj` from any that was not (the fallback arms, and
// every caller that passes a configured path straight through). Two spellings
// are two lanes: a scoped reader filtering on one of them silently drops the
// other project's rows, which are the same project's rows.
// ---------------------------------------------------------------------------

/// A path string with Windows' verbatim prefix removed: `\\?\P:\x` → `P:\x`,
/// `\\?\UNC\server\share` → `\\server\share`. Everything else is returned
/// unchanged, so this is a no-op on POSIX and on already-plain paths.
///
/// The PLAIN spelling is the canonical one, not the verbatim one: it is what
/// the user sees, what a configured tab directory is written as, and what the
/// pre-existing rows in the store already mostly carry.
pub fn plain_path(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fsutil-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        // Canonicalize so the returned root matches what `confine_*` yields and
        // comparisons don't trip over `\\?\`/8.3-name differences on Windows.
        p.canonicalize().unwrap()
    }

    /// The whole point of the predicate: BOTH platforms' absolute shapes are
    /// absolute everywhere, so the two callers (`checks::lexically_confined`,
    /// `plugins::manifest`) reach the same verdict on Windows and on Linux.
    #[test]
    fn looks_absolute_answers_for_both_platforms_at_once() {
        for abs in [
            "/etc",
            "/etc/passwd",
            "\\Windows",
            "\\\\server\\share",
            "C:\\Tools\\x",
            "c:/tools/x",
        ] {
            assert!(looks_absolute(abs), "`{abs}` must read as absolute");
        }
        for rel in ["", "src", "src/main.rs", "target\\report.xml", "./out", ".."] {
            assert!(!looks_absolute(rel), "`{rel}` must read as relative");
        }
        // Drive-RELATIVE, which is neither: `C:foo` means "foo, under whatever
        // the current directory on drive C happens to be", and `C:` names a
        // drive rather than a location. Treating either as absolute would let a
        // plugin request a grant whose target is per-drive process state.
        for drive_rel in ["C:", "C:foo", "c:tools\\acme"] {
            assert!(
                !looks_absolute(drive_rel),
                "`{drive_rel}` is drive-relative, not absolute"
            );
        }
    }

    /// Regression guard for the reason this exists: `Path::is_absolute` gives a
    /// platform-dependent answer to at least one of these, and that difference
    /// is exactly what let a refusal turn into an acceptance on the other OS.
    #[test]
    fn looks_absolute_disagrees_with_the_platform_specific_answer() {
        let foreign = if cfg!(windows) { "/etc" } else { "C:\\Windows" };
        assert!(!Path::new(foreign).is_absolute());
        assert!(
            looks_absolute(foreign),
            "`{foreign}` is absolute on the OTHER platform, which is the case \
             `Path::is_absolute` misses"
        );
    }

    #[test]
    fn existing_target_inside_is_ok() {
        let root = temp_root("inside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("f.txt"), b"x").unwrap();
        let got = confine_existing(&root, &root.join("sub").join("f.txt")).unwrap();
        assert_eq!(got, root.join("sub").join("f.txt").canonicalize().unwrap());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn existing_target_missing_is_not_found() {
        let root = temp_root("missing");
        let err = confine_existing(&root, &root.join("nope.txt")).unwrap_err();
        assert!(matches!(err, ConfineError::NotFound), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A `..` that canonicalizes out of the boundary is rejected even though
    /// the boundary's own drive is shared — this is the escape the lexical
    /// guard would also catch, verified here at the canonical layer.
    #[test]
    fn dotdot_escape_is_rejected() {
        let base = temp_root("dotdot");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside").join("secret.txt"), b"x").unwrap();
        // root/../outside/secret.txt canonicalizes to base/outside/secret.txt,
        // which is outside `root`.
        let err = confine_existing(&root, &root.join("..").join("outside").join("secret.txt"))
            .unwrap_err();
        assert!(matches!(err, ConfineError::Escaped), "got {err:?}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn nonexistent_target_walks_to_ancestor() {
        let root = temp_root("walk");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        // sub/new.txt does not exist; confinement is checked against sub.
        let got = confine_creatable(&root, &root.join("sub").join("new.txt")).unwrap();
        assert_eq!(got, Some(root.join("sub").canonicalize().unwrap()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Nothing under the target exists (`a/b/c` absent) — the walk lands on the
    /// boundary itself, which is inside the boundary, so it's confined.
    #[test]
    fn deeply_absent_target_confined_via_root() {
        let root = temp_root("deep");
        let got = confine_creatable(&root, &root.join("a").join("b").join("c")).unwrap();
        assert_eq!(got, Some(root.clone()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A not-yet-existing target whose nearest existing ancestor is OUTSIDE the
    /// boundary (via `..`) is rejected.
    #[test]
    fn creatable_target_outside_is_rejected() {
        let base = temp_root("creatable-out");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        let err =
            confine_creatable(&root, &root.join("..").join("outside").join("new.txt")).unwrap_err();
        assert!(matches!(err, ConfineError::Escaped), "got {err:?}");
        std::fs::remove_dir_all(&base).ok();
    }

    /// The join base can differ from the confinement root: a target joined onto
    /// a nested subdir still confines correctly against the outer boundary.
    #[test]
    fn base_distinct_from_root() {
        let root = temp_root("base-ne-root");
        let base = root.join("nested");
        std::fs::create_dir_all(&base).unwrap();
        // base.join(rel) built by the caller; boundary is the outer root.
        let got = confine_creatable(&root, &base.join("report.xml")).unwrap();
        assert_eq!(got, Some(base.canonicalize().unwrap()));
        // And a base-joined path that climbs out is still caught.
        let err =
            confine_creatable(&root, &base.join("..").join("..").join("escape.xml")).unwrap_err();
        assert!(matches!(err, ConfineError::Escaped), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nonexistent_boundary_is_boundary_error() {
        let root = std::env::temp_dir().join(format!("fsutil-noboundary-{}", uuid::Uuid::new_v4()));
        let err = confine_existing(&root, &root.join("f.txt")).unwrap_err();
        assert!(matches!(err, ConfineError::Boundary(_)), "got {err:?}");
    }

    /// Canonicalize resolves symlinks, so a symlink inside the boundary that
    /// points OUT of it cannot be used to escape. Symlink creation needs
    /// privileges on Windows (Developer Mode / admin), so this is unix-only;
    /// the Windows behavior is identical (`canonicalize` resolves junctions and
    /// symlinks), just not exercised here.
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let base = temp_root("symlink");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside").join("secret.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("outside"), root.join("link")).unwrap();
        let err = confine_existing(&root, &root.join("link").join("secret.txt")).unwrap_err();
        assert!(matches!(err, ConfineError::Escaped), "got {err:?}");
        std::fs::remove_dir_all(&base).ok();
    }
    // ── #104: a cwd is never a project root by itself ──────────────────────

    /// The defect's own shape: a sub-agent's shell had `cd`'d deep into the
    /// tree and every later hook reported THAT directory. Resolution must walk
    /// up to the real root, which here is marked only by cImp's own state dir.
    #[test]
    fn a_cwd_deep_in_the_tree_resolves_to_the_root_that_holds_the_state_dir() {
        let root = temp_root("root-state");
        std::fs::create_dir_all(root.join(".cimp")).unwrap();
        let cwd = root.join("src-tauri").join("src").join("harness");
        std::fs::create_dir_all(&cwd).unwrap();

        let got = find_project_root(&cwd, ".cimp").expect("a root");
        assert_eq!(got.root, root);
        assert_eq!(got.marker, RootMarker::State);
        assert_eq!(got.stray_state, None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A repo with no cImp state at all still resolves: `.git` is the marker
    /// that makes the FIRST call from a sub-directory land on the real root
    /// instead of minting a second project there.
    #[test]
    fn a_cwd_under_a_git_root_resolves_to_the_git_root() {
        let root = temp_root("root-git");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let cwd = root.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        let got = find_project_root(&cwd, ".cimp").expect("a root");
        assert_eq!(got.root, root);
        assert_eq!(got.marker, RootMarker::Vcs);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A linked worktree's `.git` is a FILE (a `gitdir:` pointer), and this
    /// repo's own fix branches live in one. `is_dir()` would miss every one of
    /// them and mint state in the worktree's sub-directories.
    #[test]
    fn a_git_file_counts_as_a_marker_so_worktrees_resolve() {
        let root = temp_root("root-worktree");
        std::fs::write(root.join(".git"), "gitdir: P:/repo/.git/worktrees/wt\n").unwrap();
        let cwd = root.join("src").join("deep");
        std::fs::create_dir_all(&cwd).unwrap();

        let got = find_project_root(&cwd, ".cimp").expect("a root");
        assert_eq!(got.root, root);
        assert_eq!(got.marker, RootMarker::Vcs);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Nearest wins: a nested repo (or submodule) is its own project, so a cwd
    /// inside it must not be attributed to the repo that contains it.
    #[test]
    fn the_nearest_git_marker_wins_over_an_outer_one() {
        let root = temp_root("root-nested");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("nested");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        let cwd = nested.join("src");
        std::fs::create_dir_all(&cwd).unwrap();

        let got = find_project_root(&cwd, ".cimp").expect("a root");
        assert_eq!(got.root, nested);
        std::fs::remove_dir_all(&root).ok();
    }

    /// No marker anywhere up the chain ⇒ no answer. The caller must fall back
    /// to a root it knows or refuse; inventing one here is how the ten stray
    /// state directories of #104 were minted.
    #[test]
    fn a_cwd_with_no_marker_anywhere_resolves_to_nothing() {
        let root = temp_root("root-bare");
        let cwd = root.join("a").join("b");
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(find_project_root(&cwd, ".cimp"), None);
        // And nothing was created on the way past.
        assert!(!cwd.join(".cimp").exists());
        assert!(!root.join(".cimp").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The retroactive half: a state directory the defect already minted must
    /// NOT capture the cwds below it. The `.git` root wins and the stray is
    /// reported by path so the user can remove it — never deleted here.
    #[test]
    fn a_state_dir_below_a_git_root_loses_and_is_reported_as_stray() {
        let root = temp_root("root-stray");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let stray_at = root.join("src-tauri").join("src").join("harness");
        std::fs::create_dir_all(stray_at.join(".cimp")).unwrap();

        let got = find_project_root(&stray_at, ".cimp").expect("a root");
        assert_eq!(got.root, root);
        assert_eq!(got.marker, RootMarker::Vcs);
        assert_eq!(got.stray_state, Some(stray_at.join(".cimp")));
        // Reported, not removed.
        assert!(stray_at.join(".cimp").is_dir());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The root's OWN state directory is not a stray — it is the state dir.
    #[test]
    fn the_roots_own_state_dir_is_never_reported_as_stray() {
        let root = temp_root("root-own-state");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".cimp")).unwrap();
        let cwd = root.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        let got = find_project_root(&cwd, ".cimp").expect("a root");
        assert_eq!(got.root, root);
        assert_eq!(got.stray_state, None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The configured subdirectory name is honoured (`graph.db_subdir` is a
    /// setting), and the default fills in for an empty one.
    #[test]
    fn the_state_marker_follows_the_configured_subdir_name() {
        let root = temp_root("root-subdir");
        std::fs::create_dir_all(root.join(".ckg")).unwrap();
        let cwd = root.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(find_project_root(&cwd, ".ckg").map(|r| r.root), Some(root.clone()));
        assert_eq!(find_project_root(&cwd, ".cimp"), None);
        // An empty setting means the default, not "match every directory".
        std::fs::create_dir_all(root.join(".cimp")).unwrap();
        assert_eq!(find_project_root(&cwd, "").map(|r| r.root), Some(root.clone()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// #104 item 5: one project, one spelling. `canonicalize` hands back the
    /// verbatim form on Windows and the raw path everywhere else, so the store
    /// held `\\?\P:\proj` and `P:\proj` for the same directory — two lanes.
    #[test]
    fn the_verbatim_and_plain_spellings_of_one_path_normalize_to_one() {
        assert_eq!(plain_path(r"\\?\P:\proj\cctts"), r"P:\proj\cctts");
        assert_eq!(plain_path(r"P:\proj\cctts"), r"P:\proj\cctts");
        assert_eq!(
            plain_path(r"\\?\P:\proj\cctts"),
            plain_path(r"P:\proj\cctts")
        );
        // UNC keeps its share form rather than losing the leading slashes.
        assert_eq!(plain_path(r"\\?\UNC\server\share\x"), r"\\server\share\x");
        // POSIX and anything already plain pass through untouched.
        assert_eq!(plain_path("/home/u/proj"), "/home/u/proj");
        assert_eq!(plain_path(""), "");
    }
}

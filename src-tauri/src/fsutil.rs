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
        let err = confine_creatable(&root, &root.join("..").join("outside").join("new.txt"))
            .unwrap_err();
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
        let err = confine_creatable(&root, &base.join("..").join("..").join("escape.xml"))
            .unwrap_err();
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
}

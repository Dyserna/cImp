use std::path::{Path, PathBuf};

fn main() {
    set_linux_origin_rpath();
    copy_espeak_data();
    copy_theming_assets();
    tauri_build::build()
}

/// On Linux the release bundles `libwebgpu_dawn.so` (ort's WebGPU execution
/// provider) next to the binary, in the portable layout's `bin/`. Windows
/// searches the executable's own directory for DLLs automatically, but the ELF
/// loader does not — without help it only searches `LD_LIBRARY_PATH` and the
/// system paths, so a portable `cimp` would fail to start with
/// "libwebgpu_dawn.so: cannot open shared object file". Add an `$ORIGIN` rpath
/// so the loader looks next to the binary, making the bundled dylib resolvable
/// with no launcher script or env var. Target-gated via `CARGO_CFG_TARGET_OS`
/// (set by cargo for the *target*, correct under cross-compilation too); a
/// no-op on Windows/macOS.
fn set_linux_origin_rpath() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
    }
}

/// Copy the repo-root `themes/`, `palettes/`, and `ebin/` folders next to the
/// built binary (`target/{profile}/themes` etc.), the same place the portable
/// release stages them (`<exe-dir>/themes`, sibling `ebin/`). The app reads
/// themes/palettes purely from disk and resolves bundled tools out of `ebin/`
/// (see `pty::resolve`) — nothing is embedded or seeded at runtime — so this
/// build-time copy is what makes dev / local builds find them without
/// hand-staging a portable layout. `ebin/` carries no binaries in the repo
/// (only a `.gitkeep`); drop a tool there to test resolution locally. The
/// release zip gets its copies from `.github/workflows/release.yml` instead.
fn copy_theming_assets() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR = target/{profile}/build/cimp-{hash}/out → up 3 = target/{profile}.
    let profile_dir = out_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("OUT_DIR has unexpected shape");
    // CARGO_MANIFEST_DIR = .../src-tauri; the repo root is one level up.
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let repo_root = manifest.parent().expect("manifest has no parent");

    for folder in ["themes", "palettes", "ebin"] {
        let src = repo_root.join(folder);
        println!("cargo:rerun-if-changed={}", src.display());
        if !src.is_dir() {
            continue;
        }
        let dst = profile_dir.join(folder);
        // Sync (overwrite + prune stale), never delete-then-copy: a copy that
        // fails partway (a file locked by a running dev instance, AV scan)
        // must not leave the destination emptier than it started. A failed
        // sync is a warning, not a build failure — the app degrades to its
        // embedded fallback theme/palette and stays usable.
        if let Err(e) = sync_dir(&src, &dst) {
            println!("cargo:warning=couldn't sync {folder}/ next to the exe ({e}); the built app may see a stale copy");
        }
    }
}

/// Copy espeak-ng-data/ (the compiled phoneme tables) next to the final binary.
/// espeak-ng's runtime auto-discovery walks the exe's parent dir for an
/// `espeak-ng-data/` folder, so this is what makes the OOV fallback work
/// without setting any env var at runtime.
///
/// Source differs by platform: on Windows, espeak-rs-sys's cmake step compiles
/// and installs the data to `out/share/espeak-ng-data`. On Linux the crate
/// builds only the library and does NOT compile the data, so we fall back to
/// the system `espeak-ng-data` package (Debian/Ubuntu: `/usr/lib/<triple>/
/// espeak-ng-data`). espeak is only the OOV fallback behind misaki's pure-Rust
/// G2P, so a missing data dir degrades that fallback rather than breaking TTS —
/// off Windows we warn instead of failing the build.
fn copy_espeak_data() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR = target/{profile}/build/cimp-{hash}/out
    // build_root = target/{profile}/build
    // profile_dir = target/{profile}
    let build_root = out_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("OUT_DIR has unexpected shape");
    let profile_dir = build_root.parent().expect("no profile dir");
    let dst = profile_dir.join("espeak-ng-data");

    match find_espeak_data(build_root).or_else(find_system_espeak_data) {
        Some(src) => {
            println!("cargo:rerun-if-changed={}", src.display());
            if let Err(e) = sync_dir(&src, &dst) {
                // A running app instance can hold espeak data files open on
                // Windows; if a usable copy is already in place, don't fail
                // the whole build over a refresh we couldn't complete.
                if dst.join("phontab").is_file() {
                    println!(
                        "cargo:warning=couldn't refresh espeak-ng-data next to the exe ({e}); \
                         keeping the existing copy (likely in use by a running instance)"
                    );
                } else {
                    panic!("failed to copy espeak-ng-data: {e}");
                }
            }
        }
        None if cfg!(windows) => {
            panic!("espeak-rs-sys did not produce espeak-ng-data; rebuild that crate");
        }
        None => {
            println!(
                "cargo:warning=espeak-ng-data not found (neither espeak-rs-sys output nor a \
                 system install); espeak OOV fallback will be unavailable. Install the \
                 `espeak-ng-data` package to bundle it."
            );
        }
    }
}

/// Locate a system-installed, compiled `espeak-ng-data` (identified by the
/// presence of `phontab`). Used on Linux, where espeak-rs-sys ships only the
/// source data, not the compiled tables. The candidate paths cover
/// Debian/Ubuntu multiarch and the common install prefixes; on Windows none
/// exist so this returns `None` and the caller's own logic applies.
fn find_system_espeak_data() -> Option<PathBuf> {
    for cand in [
        "/usr/lib/x86_64-linux-gnu/espeak-ng-data",
        "/usr/lib/aarch64-linux-gnu/espeak-ng-data",
        "/usr/share/espeak-ng-data",
        "/usr/lib/espeak-ng-data",
        "/usr/local/share/espeak-ng-data",
    ] {
        let pb = PathBuf::from(cand);
        if pb.join("phontab").is_file() {
            return Some(pb);
        }
    }
    None
}

/// Walk `target/{profile}/build/espeak-rs-sys-*/out/share/espeak-ng-data` and
/// pick the most recently built one — there can be multiple hash dirs (one per
/// build profile or invalidation), but only the latest is current.
fn find_espeak_data(build_root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(build_root).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("espeak-rs-sys-") {
            continue;
        }
        let candidate = entry.path().join("out").join("share").join("espeak-ng-data");
        if !candidate.is_dir() {
            continue;
        }
        // A failed metadata read (e.g. cargo cleaning an old hash dir
        // concurrently) skips this candidate only — `?` here would abort the
        // whole search and discard an already-found `best`.
        let Some(mtime) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if best.as_ref().map_or(true, |(t, _)| mtime > *t) {
            best = Some((mtime, candidate));
        }
    }
    best.map(|(_, p)| p)
}

/// Mirror `src` into `dst`: copy/overwrite every source entry, then prune
/// destination entries the source no longer has (so a deleted theme doesn't
/// linger next to the exe and keep getting served). Unlike delete-then-copy,
/// a copy that fails partway leaves the previous files in place rather than
/// an emptied directory. `is_dir()` on the *path* (not the dir-entry type) so
/// a symlink to a directory recurses instead of failing `fs::copy` (system
/// espeak-ng-data on Linux can contain symlinks).
fn sync_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.path().is_dir() {
            sync_dir(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    for entry in std::fs::read_dir(dst)? {
        let entry = entry?;
        if !src.join(entry.file_name()).exists() {
            let p = entry.path();
            let stale = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            if let Err(e) = stale {
                println!("cargo:warning=couldn't remove stale {} ({e})", p.display());
            }
        }
    }
    Ok(())
}

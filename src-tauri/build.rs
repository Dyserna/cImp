use std::path::{Path, PathBuf};

fn main() {
    copy_espeak_data();
    tauri_build::build()
}

/// Copy espeak-ng-data/ (built by espeak-rs-sys's cmake step) next to the
/// final binary. espeak-ng's runtime auto-discovery walks the exe's parent
/// dir for an `espeak-ng-data/` folder, so this is what makes the fallback
/// work without setting any env var at runtime.
fn copy_espeak_data() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR = target/{profile}/build/cctts-{hash}/out
    // build_root = target/{profile}/build
    // profile_dir = target/{profile}
    let build_root = out_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("OUT_DIR has unexpected shape");
    let profile_dir = build_root.parent().expect("no profile dir");

    let src = find_espeak_data(build_root)
        .expect("espeak-rs-sys did not produce espeak-ng-data; rebuild that crate");
    let dst = profile_dir.join("espeak-ng-data");

    println!("cargo:rerun-if-changed={}", src.display());
    copy_dir_all(&src, &dst).expect("failed to copy espeak-ng-data");
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
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok())?;
        if best.as_ref().map_or(true, |(t, _)| mtime > *t) {
            best = Some((mtime, candidate));
        }
    }
    best.map(|(_, p)| p)
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

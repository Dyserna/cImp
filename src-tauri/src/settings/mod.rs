//! Settings module: load/save JSON, in-memory store with broadcast updates.
//!
//! The store is the single source of truth for runtime configuration. The
//! broadcast channel propagates updates to subscribers (TTS engine, audio
//! output, processing layer, frontend). Saves are debounced (~500ms) so a
//! slider drag doesn't write the file on every frame.
//!
//! Storage is layered: the global baseline lives at `<exe-dir>/settings.json`
//! and a per-launch-directory overlay (`.ccimp.custom.config.json`) records
//! only the fields that differ from global. See `persistence` for details.

mod broadcaster;
mod migration;
mod persistence;
mod schema;

pub use broadcaster::SettingsHandle;
pub use persistence::{apply_portable_avatar_paths, load_readonly};
pub use schema::*;

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{AppError, AppResult};
use crate::shell::ShellSpec;

/// Write `bytes` to `path` atomically: write to a sibling temp file, fsync,
/// then rename over the target. A crash or power loss between the temp
/// write and rename leaves the original file intact (the rename is the
/// commit point). This is the only way to write user-configuration files
/// without risking truncation on the 500 ms debounced save path. Used for
/// both the global baseline and the per-folder overlay.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    // Each write gets a UNIQUE temp name. Two writers can legitimately
    // target the same file concurrently — e.g. the 500 ms debounced saver
    // and `load()`'s repair-save both touch the per-folder overlay — and a
    // shared `<name>.tmp` lets them truncate each other's temp mid-write or
    // race on the rename (NotFound on the loser; sharing-violation on
    // Windows where the destination/temp may still be open). A per-write
    // suffix makes each writer's temp private; the rename is still the
    // atomic commit point and last-writer-wins on the final file.
    let tmp = match path.file_name() {
        Some(name) => {
            let mut tmp_name = name.to_os_string();
            tmp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
            path.with_file_name(tmp_name)
        }
        None => return Err(AppError::Settings(format!("path has no file name: {}", path.display()))),
    };

    {
        let mut f = fs::File::create(&tmp).map_err(AppError::Io)?;
        f.write_all(bytes).map_err(AppError::Io)?;
        f.sync_all().map_err(AppError::Io)?;
    }

    // On Unix, restrict the file to owner read/write only — settings
    // contains the plaintext local-LLM auth_token and any per-tab env
    // values the user defined, which may include credentials. Windows
    // inherits ACLs from the parent directory; programmatic hardening
    // there is documented as a follow-up in
    // docs/FUTURE-FEATURES-keyring.md.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        if let Err(e) = fs::set_permissions(&tmp, perms) {
            tracing::warn!(error = %e, path = %tmp.display(), "settings: chmod 0600 failed");
        }
    }

    if let Err(e) = fs::rename(&tmp, path) {
        // Rename failed — clean up the temp so it doesn't accumulate, then
        // surface the original error.
        let _ = fs::remove_file(&tmp);
        return Err(AppError::Io(e));
    }
    Ok(())
}

/// Bring up the settings store from disk (or defaults). Always succeeds —
/// missing/corrupt files are recovered with defaults; v1 / v1.1 files are
/// migrated to v1.2 and a backup is written. The default shell is needed
/// to fill in the platform-specific Shell-1 entry on fresh installs and
/// when migration consumes the legacy `_shell_1_tmp` interim key.
///
/// `launch_cwd` is the directory ccimp was started in. The custom overlay,
/// if any, is read from and written to that directory.
pub fn init(default_shell: &ShellSpec, launch_cwd: &Path) -> SettingsHandle {
    let outcome = persistence::load(default_shell, launch_cwd);
    SettingsHandle::new(outcome.settings, outcome.global, launch_cwd.to_path_buf())
}

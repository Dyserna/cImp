//! JSON load/save with corruption recovery. The on-disk format is a single
//! JSON object matching `Settings`. If the file is missing we write defaults;
//! if it parses to garbage we overwrite with defaults so the next load is
//! clean. Either way `load` always returns a usable `Settings`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::settings::schema::Settings;

const FILE_NAME: &str = "settings.json";

/// `%APPDATA%\cctts\settings.json` on Windows; the analogous config dir on
/// other platforms.
pub fn config_path() -> AppResult<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| AppError::Settings("no config dir on this platform".into()))?;
    Ok(dir.join("cctts").join(FILE_NAME))
}

/// Always returns a `Settings`. Defaults are written to disk when the file
/// is absent or corrupt so subsequent loads see a clean file.
pub fn load() -> Settings {
    let path = match config_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "settings: cannot resolve config path; using defaults");
            return Settings::default();
        }
    };

    if !path.exists() {
        let s = Settings::default();
        if let Err(e) = save_to(&path, &s) {
            tracing::warn!(error = %e, path = %path.display(), "settings: write defaults failed");
        } else {
            tracing::info!(path = %path.display(), "settings: wrote defaults");
        }
        return s;
    }

    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Settings>(&text) {
            Ok(s) => {
                tracing::info!(path = %path.display(), "settings: loaded");
                s
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "settings: parse failed; reverting to defaults"
                );
                let s = Settings::default();
                if let Err(e) = save_to(&path, &s) {
                    tracing::warn!(error = %e, "settings: rewriting defaults failed");
                }
                s
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read failed; using defaults");
            Settings::default()
        }
    }
}

pub fn save(settings: &Settings) -> AppResult<()> {
    let path = config_path()?;
    save_to(&path, settings)
}

fn save_to(path: &Path, settings: &Settings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Settings(format!("serialize: {e}")))?;
    fs::write(path, text).map_err(AppError::Io)
}

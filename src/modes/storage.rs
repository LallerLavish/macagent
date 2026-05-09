use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

use super::definition::{validate_name, Mode};
use super::error::ModeError;

const MODES_SUBDIR: &str = "macagent/modes";

/// Returns ~/.config/macagent/modes (created if missing)
pub async fn modes_dir() -> Result<PathBuf, ModeError> {
    let base = dirs::config_dir().ok_or(ModeError::NoConfigDir)?;
    let dir = base.join(MODES_SUBDIR);
    fs::create_dir_all(&dir).await
        .map_err(|e| ModeError::io(&dir, e))?;
    Ok(dir)
}

fn path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.toml", name))
}

pub async fn exists(name: &str) -> Result<bool, ModeError> {
    validate_name(name)?;
    let dir = modes_dir().await?;
    let path = path_for(&dir, name);
    Ok(fs::try_exists(&path).await.map_err(|e| ModeError::io(&path, e))?)
}

pub async fn read(name: &str) -> Result<Mode, ModeError> {
    validate_name(name)?;
    let dir = modes_dir().await?;
    let path = path_for(&dir, name);

    let content = match fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ModeError::NotFound(name.into()));
        }
        Err(e) => return Err(ModeError::io(&path, e)),
    };

    toml::from_str(&content).map_err(|source| ModeError::TomlParse { path, source })
}

pub async fn write(mode: &Mode) -> Result<(), ModeError> {
    validate_name(&mode.name)?;
    let dir = modes_dir().await?;
    let path = path_for(&dir, &mode.name);

    let content = toml::to_string_pretty(mode)?;

    // Atomic write: write to tmp then rename
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, content).await.map_err(|e| ModeError::io(&tmp, e))?;
    fs::rename(&tmp, &path).await.map_err(|e| ModeError::io(&path, e))?;

    debug!(mode = %mode.name, path = ?path, "mode written");
    Ok(())
}

pub async fn delete(name: &str) -> Result<(), ModeError> {
    validate_name(name)?;
    let dir = modes_dir().await?;
    let path = path_for(&dir, name);

    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ModeError::NotFound(name.into()))
        }
        Err(e) => Err(ModeError::io(&path, e)),
    }
}

pub async fn list() -> Result<Vec<String>, ModeError> {
    let dir = modes_dir().await?;
    let mut entries = fs::read_dir(&dir).await.map_err(|e| ModeError::io(&dir, e))?;
    let mut names = Vec::new();

    while let Some(entry) = entries.next_entry().await
        .map_err(|e| ModeError::io(&dir, e))?
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            // Skip names that don't pass validation (e.g., tmp files, bad names)
            if validate_name(stem).is_ok() {
                names.push(stem.to_string());
            }
        }
    }

    names.sort();
    Ok(names)
}
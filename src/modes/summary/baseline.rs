//! Persists per-repo git baseline at end of session.
//!
//! Saved on `leave work mode`. Read on `switch to work mode` to compute
//! "what changed since last session." Lives at:
//! ~/Library/Application Support/macagent/state/git_baseline.toml

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

use crate::modes::error::ModeError;

const BASELINE_FILENAME: &str = "git_baseline.toml";
const STATE_SUBDIR: &str = "macagent/state";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBaseline {
    pub saved_at: DateTime<Utc>,
    pub repos: Vec<RepoBaseline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoBaseline {
    pub path: String,
    pub head: String,
    pub branch: String,
    /// Working-tree diff at save time, possibly truncated.
    pub diff_at_save: String,
    /// Last commit message at save time.
    pub last_commit: String,
}

async fn baseline_path() -> Result<PathBuf, ModeError> {
    let base = dirs::config_dir().ok_or(ModeError::NoConfigDir)?;
    let dir = base.join(STATE_SUBDIR);
    fs::create_dir_all(&dir).await
        .map_err(|e| ModeError::io(&dir, e))?;
    Ok(dir.join(BASELINE_FILENAME))
}

pub async fn write(baseline: &GitBaseline) -> Result<(), ModeError> {
    let path = baseline_path().await?;
    let tmp = path.with_extension("toml.tmp");
    let content = toml::to_string_pretty(baseline)?;
    fs::write(&tmp, content).await.map_err(|e| ModeError::io(&tmp, e))?;
    fs::rename(&tmp, &path).await.map_err(|e| ModeError::io(&path, e))?;
    debug!(?path, repos = baseline.repos.len(), "git baseline written");
    Ok(())
}

pub async fn read() -> Result<Option<GitBaseline>, ModeError> {
    let path = baseline_path().await?;
    if !fs::try_exists(&path).await.map_err(|e| ModeError::io(&path, e))? {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).await
        .map_err(|e| ModeError::io(&path, e))?;
    let baseline: GitBaseline = toml::from_str(&content)
        .map_err(|source| ModeError::TomlParse { path, source })?;
    Ok(Some(baseline))
}

pub async fn delete() -> Result<(), ModeError> {
    let path = baseline_path().await?;
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ModeError::io(&path, e)),
    }
}

/// Look up a specific repo's baseline by path. Returns None if not in baseline.
pub fn find_repo<'a>(baseline: &'a GitBaseline, path: &Path) -> Option<&'a RepoBaseline> {
    let p = path.to_string_lossy();
    baseline.repos.iter().find(|r| r.path == p)
}
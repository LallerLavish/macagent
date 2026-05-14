//! Persists per-repo, per-branch git baseline at end of session.
//!
//! Saved on `leave work mode`. Read on `switch to work mode` to compute
//! "what changed since last session." Each entry is keyed by (repo_id, branch)
//! so switching branches preserves prior state. Lives at:
//! ~/Library/Application Support/macagent/state/git_baseline.toml

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tracing::debug;

use crate::modes::error::ModeError;

const BASELINE_FILENAME: &str = "git_baseline.toml";
const STATE_SUBDIR: &str = "macagent/state";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBaseline {
    pub saved_at: DateTime<Utc>,
    pub entries: Vec<RepoBaseline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoBaseline {
    /// Stable identifier: SHA of the repo's first commit.
    /// Survives directory moves and renames.
    pub repo_id: String,
    /// Last-known filesystem path. Updated on every write.
    /// For display/debug only — never use as a lookup key.
    pub repo_path: String,
    /// Branch name at save time. Detached HEAD entries are never written.
    pub branch: String,
    /// HEAD commit SHA at save time.
    pub head: String,
    /// Working-tree diff at save time, possibly truncated.
    pub diff_at_save: String,
    /// Last commit subject at save time.
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
    debug!(?path, entries = baseline.entries.len(), "git baseline written");
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

/// Look up a specific repo's baseline by (repo_id, branch).
/// Returns None if no entry exists for that combination.
pub fn find_entry<'a>(
    baseline: &'a GitBaseline,
    repo_id: &str,
    branch: &str,
) -> Option<&'a RepoBaseline> {
    baseline.entries.iter()
        .find(|e| e.repo_id == repo_id && e.branch == branch)
}

/// Upsert an entry by (repo_id, branch). Replaces existing entry if one
/// exists with the same key, otherwise appends. Also updates `repo_path`
/// on existing entries so it reflects the latest known location.
pub fn upsert_entry(baseline: &mut GitBaseline, entry: RepoBaseline) {
    if let Some(existing) = baseline.entries.iter_mut()
        .find(|e| e.repo_id == entry.repo_id && e.branch == entry.branch)
    {
        *existing = entry;
    } else {
        baseline.entries.push(entry);
    }
}
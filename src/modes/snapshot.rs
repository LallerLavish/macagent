//! Pre-work-mode snapshot — ephemeral runtime state, not a saved mode.
//!
//! Stored separately from modes/ to make the distinction obvious.
//! The snapshot persists across daemon restarts (per Q1=A) so that a crash
//! mid-work-mode is recoverable — the user invokes `leave work mode` and
//! the snapshot is applied.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, warn};

use super::error::ModeError;
use super::system_state::SystemState;

const STATE_SUBDIR: &str = "macagent/state";
const SNAPSHOT_FILENAME: &str = "pre_work_snapshot.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreWorkSnapshot {
    pub captured_at: DateTime<Utc>,
    pub mode_name: String,
    pub apps: SnapshotApps,
    pub system: SystemState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotApps {
    pub running: Vec<String>,  // bundle IDs that were running pre-work
}

async fn state_dir() -> Result<PathBuf, ModeError> {
    let base = dirs::config_dir().ok_or(ModeError::NoConfigDir)?;
    let dir = base.join(STATE_SUBDIR);
    fs::create_dir_all(&dir).await
        .map_err(|e| ModeError::io(&dir, e))?;
    Ok(dir)
}

async fn snapshot_path() -> Result<PathBuf, ModeError> {
    Ok(state_dir().await?.join(SNAPSHOT_FILENAME))
}

pub async fn exists() -> Result<bool, ModeError> {
    let path = snapshot_path().await?;
    fs::try_exists(&path).await.map_err(|e| ModeError::io(&path, e))
}

/// Atomic write — tmp + rename, same as mode storage.
pub async fn write(snap: &PreWorkSnapshot) -> Result<(), ModeError> {
    let path = snapshot_path().await?;
    let tmp = path.with_extension("toml.tmp");
    let content = toml::to_string_pretty(snap)?;
    fs::write(&tmp, content).await.map_err(|e| ModeError::io(&tmp, e))?;
    fs::rename(&tmp, &path).await.map_err(|e| ModeError::io(&path, e))?;
    debug!(path = ?path, "snapshot written");
    Ok(())
}

pub async fn read() -> Result<PreWorkSnapshot, ModeError> {
    let path = snapshot_path().await?;
    let content = match fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ModeError::NotFound("snapshot".into()));
        }
        Err(e) => return Err(ModeError::io(&path, e)),
    };
    toml::from_str(&content).map_err(|source| ModeError::TomlParse { path, source })
}

pub async fn delete() -> Result<(), ModeError> {
    let path = snapshot_path().await?;
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),  // already gone
        Err(e) => Err(ModeError::io(&path, e)),
    }
}

/// Called at daemon startup. If a stale snapshot exists, log a warning
/// telling the user what state they're in. Don't auto-revert — let the
/// user invoke `leave work mode` explicitly.
pub async fn check_orphaned() -> Result<(), ModeError> {
    if !exists().await? {
        return Ok(());
    }
    match read().await {
        Ok(snap) => {
            warn!(
                mode = %snap.mode_name,
                captured = %snap.captured_at,
                "orphaned work-mode snapshot detected — run 'leave work mode' to revert"
            );
        }
        Err(e) => {
            // Corrupt snapshot. Delete it so the user isn't stuck.
            warn!(error = %e, "snapshot corrupt, deleting");
            let _ = delete().await;
        }
    }
    Ok(())
}
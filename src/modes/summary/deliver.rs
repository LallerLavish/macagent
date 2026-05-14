//! Deliver generated summary to a file the user can read AND persist it
//! to SQLite for retrieval at future summary-generation time.
//!
//! File on disk = human-readable artifact (user can open the .txt).
//! SQLite = source of truth for retrieval.

use chrono::Utc;
use std::path::PathBuf;
use tokio::fs;
use tracing::info;

use crate::modes::error::ModeError;

use super::db::{self, NewSummary};
use super::parse::parse_summary;

const SUMMARIES_SUBDIR: &str = "macagent/summaries";

/// All fields needed to deliver and persist a summary.
pub struct DeliverInput {
    pub mode_name: String,
    pub repo_id: String,
    pub repo_path: String,
    pub branch: String,
    pub commit_sha: String,
    pub files_touched: Vec<String>,
    pub summary_text: String,
}

pub async fn write_summary(input: DeliverInput) -> Result<PathBuf, ModeError> {
    let base = dirs::config_dir().ok_or(ModeError::NoConfigDir)?;
    let dir = base.join(SUMMARIES_SUBDIR);
    fs::create_dir_all(&dir).await.map_err(|e| ModeError::io(&dir, e))?;

    let now = Utc::now();
    let timestamp_str = now.format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("{}_{}.txt", timestamp_str, input.mode_name);
    let path = dir.join(&filename);

    // Write the human-readable file first. If this fails, no DB write.
    fs::write(&path, &input.summary_text)
        .await
        .map_err(|e| ModeError::io(&path, e))?;
    info!(?path, "summary file written");

    // Parse into headline + body. Works for both current adapter (raw blob)
    // and post-retrain output (HEADLINE:/DETAILS: structured).
    let (headline, body) = parse_summary(&input.summary_text);

    let new = NewSummary {
        repo_id: input.repo_id,
        repo_path: input.repo_path,
        branch: input.branch,
        commit_sha: input.commit_sha,
        mode_name: input.mode_name,
        timestamp: now.timestamp(),
        headline,
        body,
        file_path: path.to_string_lossy().into_owned(),
        files_touched: input.files_touched,
    };

    match db::insert_summary(new).await {
        Ok(id) => info!(summary_id = id, "summary persisted to db"),
        Err(e) => {
            // DB failure is non-fatal — file is still on disk.
            tracing::warn!(error = %e, "failed to persist summary to db");
        }
    }

    Ok(path)
}

/// Path to the most recent summary file. Queries the DB instead of
/// scanning the filesystem.
pub async fn most_recent() -> Result<Option<PathBuf>, ModeError> {
    let row = db::most_recent_summary().await?;
    Ok(row.map(|r| PathBuf::from(r.file_path)))
}
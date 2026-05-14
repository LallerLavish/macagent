//! Summary feature: gather git activity context for LLM-based session
//! summaries. Stage 1 (this) gathers and renders the input. Stage 4 will
//! send it to the LLM and return the summary text.

pub mod baseline;
pub mod git;
pub mod input;
pub mod repos;
pub mod llm;
pub mod prepare_input;
pub mod deliver;
pub mod parse;
pub mod db;

use chrono::Utc;
use tracing::{info, warn};

use crate::modes::error::ModeError;
use crate::modes::storage;

pub use baseline::{GitBaseline, RepoBaseline};
pub use repos::{DetectedRepo};
pub use git::BranchState;
pub use input::SummaryInput;

/// Save a git baseline for all detected repos in the given mode.
/// Called at end of `leave work mode`.
pub async fn save_baseline_for_mode(mode_name: &str) -> Result<usize, ModeError> {
    let mode = storage::read(mode_name).await?;
    let repos = repos::detect_repos(&mode).await;
    if repos.is_empty() {
        info!("no git repos detected — skipping baseline save");
        return Ok(0);
    }

    // Read existing baseline so we preserve entries for branches/repos
    // we didn't touch this session. New design: keyed by (repo_id, branch).
    let mut baseline = baseline::read().await?.unwrap_or_else(|| GitBaseline {
        saved_at: Utc::now(),
        entries: Vec::new(),
    });

    let mut updated = 0;
    for repo in &repos {
        match build_repo_baseline(repo).await {
            Ok(Some(rb)) => {
                baseline::upsert_entry(&mut baseline, rb);
                updated += 1;
            }
            Ok(None) => {
                // Skipped (detached HEAD or similar) — already logged inside
            }
            Err(e) => warn!(
                repo_id = %repo.repo_id,
                root = %repo.root.display(),
                error = %e,
                "skipping repo in baseline"
            ),
        }
    }

    if updated == 0 {
        info!("no baselines updated this session");
        return Ok(0);
    }

    baseline.saved_at = Utc::now();
    baseline::write(&baseline).await?;
    info!(updated, total_entries = baseline.entries.len(), "git baseline saved");
    Ok(updated)
}

async fn build_repo_baseline(
    repo: &DetectedRepo,
) -> Result<Option<RepoBaseline>, git::GitError> {
    // Skip detached HEAD: no stable branch to key the entry against.
    // Unborn shouldn't reach here (filtered in detect_repos), defensive only.
    let branch_name = match &repo.branch {
        BranchState::Named(name) => name.clone(),
        BranchState::Detached(marker) => {
            info!(
                repo_id = %repo.repo_id,
                root = %repo.root.display(),
                marker = %marker,
                "skipping baseline for detached HEAD"
            );
            return Ok(None);
        }
        BranchState::Unborn => return Ok(None),
    };

    let head = git::current_head(&repo.root).await?;
    let last_commit = git::last_commit_subject(&repo.root).await.unwrap_or_default();
    let diff = git::working_diff(&repo.root).await.unwrap_or_default();
    let truncated = git::truncate_diff(&diff, 50_000);  // ~12k tokens, generous

    Ok(Some(RepoBaseline {
        repo_id: repo.repo_id.clone(),
        repo_path: repo.root.to_string_lossy().into_owned(),
        branch: branch_name,
        head,
        diff_at_save: truncated,
        last_commit,
    }))
}

/// Build the full summary input string for the active mode. Used both for
/// debug printing (Stage 1 deliverable) and for sending to the LLM (Stage 4).
pub async fn build_summary_text(mode_name: &str) -> Result<String, ModeError> {
    let mode = storage::read(mode_name).await?;
    let repos = repos::detect_repos(&mode).await;
    let summary_input = input::build(&repos).await;
    Ok(input::render(&summary_input))
}

pub async fn generate_and_deliver(mode_name: &str) -> Result<std::path::PathBuf, String> {
    let mode = storage::read(mode_name).await
        .map_err(|e| format!("read mode: {e}"))?;

    let prepared = match prepare_input::prepare(&mode).await {
        Some(p) => p,
        None => return Err("no active repo to summarize".to_string()),
    };

    info!(
        repo_id = %prepared.repo_id,
        repo = ?prepared.active_repo,
        branch = %prepared.branch,
        files_touched = prepared.files_touched.len(),
        "generating summary"
    );

    let summary_text = llm::generate_summary(&prepared.prompt).await
        .map_err(|e| format!("inference: {e}"))?;

    let path = deliver::write_summary(deliver::DeliverInput {
        mode_name: mode_name.to_string(),
        repo_id: prepared.repo_id,
        repo_path: prepared.active_repo.to_string_lossy().into_owned(),
        branch: prepared.branch,
        commit_sha: prepared.commit_sha,
        files_touched: prepared.files_touched,
        summary_text,
    })
    .await
    .map_err(|e| format!("deliver: {e}"))?;

    Ok(path)
}
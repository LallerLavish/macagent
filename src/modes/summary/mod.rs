//! Summary feature: gather git activity context for LLM-based session
//! summaries. Stage 1 (this) gathers and renders the input. Stage 4 will
//! send it to the LLM and return the summary text.

pub mod baseline;
pub mod git;
pub mod input;
pub mod repos;

use chrono::Utc;
use tracing::{info, warn};

use crate::modes::error::ModeError;
use crate::modes::storage;

pub use baseline::{GitBaseline, RepoBaseline};
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

    let mut entries = Vec::new();
    for repo in &repos {
        match build_repo_baseline(repo).await {
            Ok(rb) => entries.push(rb),
            Err(e) => warn!(repo = ?repo, error = %e, "skipping repo in baseline"),
        }
    }

    let baseline = GitBaseline {
        saved_at: Utc::now(),
        repos: entries,
    };

    let count = baseline.repos.len();
    baseline::write(&baseline).await?;
    info!(repos = count, "git baseline saved");
    Ok(count)
}

async fn build_repo_baseline(
    repo: &std::path::Path,
) -> Result<RepoBaseline, git::GitError> {
    let head = git::current_head(repo).await?;
    let branch = git::current_branch(repo).await?;
    let last_commit = git::last_commit_subject(repo).await.unwrap_or_default();
    let diff = git::working_diff(repo).await.unwrap_or_default();
    let truncated = git::truncate_diff(&diff, 50_000);  // ~12k tokens, generous

    Ok(RepoBaseline {
        path: repo.to_string_lossy().into_owned(),
        head,
        branch,
        diff_at_save: truncated,
        last_commit,
    })
}

/// Build the full summary input string for the active mode. Used both for
/// debug printing (Stage 1 deliverable) and for sending to the LLM (Stage 4).
pub async fn build_summary_text(mode_name: &str) -> Result<String, ModeError> {
    let mode = storage::read(mode_name).await?;
    let repos = repos::detect_repos(&mode).await;
    let summary_input = input::build(&repos).await;
    Ok(input::render(&summary_input))
}
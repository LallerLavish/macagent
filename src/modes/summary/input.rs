//! Build a structured input string for the LLM summary.
//!
//! For each detected repo:
//!   - Compare current state to saved baseline (if any)
//!   - Build a section like:
//!       PROJECT: macagent
//!       BRANCH: main
//!       LAST KNOWN STATE: ... (from baseline)
//!       WHAT'S CHANGED SINCE: ... (commits + diff)
//!       CURRENT WORKING DIFF: ...
//!
//! Output is concatenated repo sections + metadata header.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::fmt::Write;
use tracing::warn;

use super::baseline::{self, GitBaseline};
use super::git;

const DIFF_TRUNCATE_CHARS: usize = 8_000;  // ~2k tokens
const COMMITS_TRUNCATE_LINES: usize = 30;

pub struct SummaryInput {
    pub gap_hours: Option<f64>,
    pub baseline_saved_at: Option<DateTime<Utc>>,
    pub current_at: DateTime<Utc>,
    pub repos: Vec<RepoSection>,
}

pub struct RepoSection {
    pub path: PathBuf,
    pub branch: String,
    pub baseline: Option<RepoBaselineSummary>,
    pub commits_since_baseline: String,
    pub working_diff_now: String,
    pub diff_stat_now: String,
}

pub struct RepoBaselineSummary {
    pub head_short: String,
    pub last_commit: String,
    pub diff_at_save: String,
}

/// Build the full summary input by gathering current git state for each
/// repo and pairing it with baseline data (if available).
pub async fn build(repos: &[PathBuf]) -> SummaryInput {
    let baseline_opt = baseline::read().await.unwrap_or_else(|e| {
        warn!(error = %e, "failed to read git baseline, proceeding without it");
        None
    });
    let now = Utc::now();
    let baseline_saved_at = baseline_opt.as_ref().map(|b| b.saved_at);
    let gap_hours = baseline_saved_at.map(|t| {
        let delta = now.signed_duration_since(t);
        delta.num_seconds() as f64 / 3600.0
    });

    let mut sections = Vec::new();
    for repo in repos {
        match build_repo_section(repo, baseline_opt.as_ref()).await {
            Ok(s) => sections.push(s),
            Err(e) => warn!(repo = ?repo, error = %e, "skipping repo in summary"),
        }
    }

    SummaryInput {
        gap_hours,
        baseline_saved_at,
        current_at: now,
        repos: sections,
    }
}

async fn build_repo_section(
    repo: &Path,
    baseline_opt: Option<&GitBaseline>,
) -> Result<RepoSection, git::GitError> {
    let branch = git::current_branch(repo).await?;
    let head = git::current_head(repo).await?;
    let working_diff = git::working_diff(repo).await.unwrap_or_default();
    let stat = git::diff_stat(repo).await.unwrap_or_default();

    let baseline_summary = baseline_opt
        .and_then(|b| baseline::find_repo(b, repo))
        .map(|r| RepoBaselineSummary {
            head_short: r.head.chars().take(8).collect(),
            last_commit: r.last_commit.clone(),
            diff_at_save: r.diff_at_save.clone(),
        });

    let commits_since = if let Some(b) = baseline_opt
        .and_then(|b| baseline::find_repo(b, repo))
    {
        if b.head != head {
            git::commits_since(repo, &b.head).await.unwrap_or_default()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    Ok(RepoSection {
        path: repo.to_path_buf(),
        branch,
        baseline: baseline_summary,
        commits_since_baseline: truncate_lines(&commits_since, COMMITS_TRUNCATE_LINES),
        working_diff_now: git::truncate_diff(&working_diff, DIFF_TRUNCATE_CHARS),
        diff_stat_now: stat,
    })
}

fn truncate_lines(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.to_string();
    }
    let omitted = lines.len() - max_lines;
    let mut out = lines[..max_lines].join("\n");
    write!(out, "\n... [{omitted} more commits] ...").ok();
    out
}

/// Render the SummaryInput as a flat text blob suitable for LLM input.
/// This is the prompt-ready format.
pub fn render(input: &SummaryInput) -> String {
    let mut out = String::new();

    // Header
    if let Some(gap) = input.gap_hours {
        let _ = write!(out, "TIME SINCE LAST SESSION: {:.1} hours\n", gap);
    } else {
        out.push_str("TIME SINCE LAST SESSION: unknown (no previous baseline)\n");
    }

    if input.repos.is_empty() {
        out.push_str("\nNO ACTIVE REPOS DETECTED.\n");
        return out;
    }

    for repo in &input.repos {
        let _ = write!(out, "\n========== PROJECT: {} ==========\n", repo.path.display());
        let _ = write!(out, "BRANCH: {}\n", repo.branch);

        if let Some(b) = &repo.baseline {
            let _ = write!(out, "\nLAST KNOWN STATE (from previous session):\n");
            let _ = write!(out, "  HEAD: {}\n", b.head_short);
            let _ = write!(out, "  Last commit: {}\n", b.last_commit);
            if !b.diff_at_save.trim().is_empty() {
                let _ = write!(out, "  Uncommitted at save:\n{}\n", indent(&b.diff_at_save, "    "));
            } else {
                out.push_str("  Working tree was clean at save.\n");
            }
        } else {
            out.push_str("\n(No baseline saved — first time summarizing this repo)\n");
        }

        if !repo.commits_since_baseline.trim().is_empty() {
            let _ = write!(
                out,
                "\nCOMMITS SINCE BASELINE:\n{}\n",
                repo.commits_since_baseline
            );
        }

        if !repo.diff_stat_now.trim().is_empty() {
            let _ = write!(out, "\nCURRENT WORKING DIFF (stat):\n{}\n", repo.diff_stat_now);
            let _ = write!(out, "\nCURRENT WORKING DIFF:\n{}\n", repo.working_diff_now);
        } else {
            out.push_str("\nCURRENT WORKING TREE: clean\n");
        }
    }

    out.push_str("\n\nWrite a 3-4 sentence summary of where the user left off, what's pending, and what they likely want to do next. Be specific — reference filenames and what changed. Don't be generic.\n");

    out
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n")
}
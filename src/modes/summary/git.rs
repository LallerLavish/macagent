//! Read-only git command wrappers. Each function shells out to system git
//! and returns parsed output. All commands use `git -C <path>` so we never
//! mutate working directory.

use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

const GIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git command failed: {0}")]
    CommandFailed(String),

    #[error("git timed out: {0}")]
    Timeout(String),

    #[error("subprocess spawn failed: {0}")]
    Spawn(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchState {
    Named(String),
    Detached(String),  // includes "detached@<short-sha>" marker
    Unborn,
}

/// Run `git -C <path> <args...>`. Returns stdout on success.
async fn run_git(path: &Path, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path);
    for a in args {
        cmd.arg(a);
    }

    let output = timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| GitError::Timeout(format!("git {:?} in {:?}", args, path)))?
        .map_err(|e| GitError::Spawn(e.to_string()))?;

    if !output.status.success() {
        return Err(GitError::CommandFailed(format!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// True if the path is inside a git working tree.
pub async fn is_git_repo(path: &Path) -> bool {
    matches!(
        run_git(path, &["rev-parse", "--is-inside-work-tree"]).await,
        Ok(s) if s.trim() == "true"
    )
}

/// Walk up the directory tree from `path` looking for a `.git` directory.
/// Returns the repo root (the directory containing `.git`).
pub fn find_repo_root(path: &Path) -> Option<std::path::PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub async fn current_head(path: &Path) -> Result<String, GitError> {
    Ok(run_git(path, &["rev-parse", "HEAD"]).await?.trim().to_string())
}

/// Determine the current branch state of a repo.
///
/// Returns BranchState::Named for normal branches, BranchState::Detached
/// (with a short-SHA marker) for detached HEAD, and BranchState::Unborn
/// when the repo has no commits yet.
///
/// Never errors — all failure modes resolve to a meaningful BranchState
/// variant so downstream code can pattern-match without Result handling.
pub async fn current_branch(path: &Path) -> BranchState {
    // Try symbolic-ref first: succeeds only on a named branch
    if let Ok(out) = run_git(path, &["symbolic-ref", "--short", "HEAD"]).await {
        let name = out.trim();
        if !name.is_empty() {
            return BranchState::Named(name.to_string());
        }
    }

    // symbolic-ref failed → detached HEAD or unborn.
    // If HEAD resolves to a SHA, it's detached. Otherwise unborn.
    match run_git(path, &["rev-parse", "HEAD"]).await {
        Ok(sha_out) => {
            let sha = sha_out.trim();
            if sha.is_empty() {
                BranchState::Unborn
            } else {
                let short = &sha[..sha.len().min(7)];
                BranchState::Detached(format!("detached@{}", short))
            }
        }
        Err(_) => BranchState::Unborn,
    }
}
/// Compute the stable repo identifier — the SHA of the first commit.
///
/// Stable across directory moves, branch renames, and working tree changes.
/// Returns None if the repo has no commits (unborn).
pub async fn repo_id(path: &Path) -> Option<String> {
    let out = run_git(path, &["rev-list", "--max-parents=0", "HEAD"])
        .await
        .ok()?;
    let first_line = out.lines().next()?.trim();
    if first_line.is_empty() {
        None
    } else {
        // If multiple root commits exist (octopus merges of unrelated
        // histories — rare), take the first. Still stable for repo lifetime.
        Some(first_line.to_string())
    }
}

/// Last commit subject line (just the message of HEAD).
pub async fn last_commit_subject(path: &Path) -> Result<String, GitError> {
    Ok(run_git(path, &["log", "-1", "--pretty=%s"])
        .await?
        .trim()
        .to_string())
}

/// `git log <since>..HEAD --oneline`. If `since` equals current HEAD, returns empty.
pub async fn commits_since(path: &Path, since: &str) -> Result<String, GitError> {
    run_git(path, &["log", &format!("{since}..HEAD"), "--oneline"]).await
}

/// Working-tree diff vs HEAD. Returns the full diff text.
pub async fn working_diff(path: &Path) -> Result<String, GitError> {
    run_git(
        path,
        &["diff", "--diff-filter=ACMR"],  // skip binaries / deletions of binaries
    )
    .await
}

pub async fn diff_stat(path: &Path) -> Result<String, GitError> {
    Ok(run_git(path, &["diff", "--stat"]).await?.trim().to_string())
}

pub async fn status_porcelain(path: &Path) -> Result<String, GitError> {
    run_git(path, &["status", "--porcelain"]).await
}

/// Diff of the most recent commit (HEAD vs HEAD~1).
/// Used as fallback when working tree is clean.
pub async fn run_diff_for_last_commit(path: &Path) -> Result<String, GitError> {
    run_git(path, &["log", "-1", "-p", "--diff-filter=ACMR"]).await
}

/// Truncate a diff to fit within a token budget. Keeps head and tail with
/// a marker in the middle. Threshold is in characters (rough proxy for tokens
/// — most diffs are ~3-4 chars per token).
pub fn truncate_diff(diff: &str, max_chars: usize) -> String {
    if diff.len() <= max_chars {
        return diff.to_string();
    }
    // Keep ~70% from the start, ~30% from the end. Head is usually more
    // informative (file names, first changes); tail catches recent edits.
    let head_chars = (max_chars * 7) / 10;
    let tail_chars = max_chars - head_chars - 80;  // 80 chars for marker

    let head: String = diff.chars().take(head_chars).collect();
    let tail: String = diff
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let omitted = diff.len() - head.len() - tail.len();
    format!(
        "{head}\n\n... [{omitted} chars omitted] ...\n\n{tail}"
    )
}



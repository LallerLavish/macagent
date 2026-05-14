//! Build the LLM input for a session summary.
//!
//! Picks the "active" repo, extracts last commit + status + diff, applies
//! smart truncation, formats per the trained model's expected schema:
//!
//!   [LAST COMMIT]: <subject>
//!   [STATUS]: <git status --porcelain output>
//!   [DIFF]: <truncated diff>
//!
//! Wrapped in Qwen's chat template with the trained system prompt.

use std::path::PathBuf;
use std::time::SystemTime;
use tracing::{debug, info};

use crate::modes::Mode;

use super::git::{self, BranchState};
use super::repos::{detect_repos, DetectedRepo};

/// System prompt — must match what the model was trained on.
const SYSTEM_PROMPT: &str = "You are a senior macOS developer summarizing a coding session. Read the session payload and output a 3-5 sentence summary naming specific files and the immediate next step. Do not use pleasantries.";

/// Target token budget for the [DIFF] section. Rough estimate: 4 chars/token.
const DIFF_CHAR_BUDGET: usize = 2000;

/// Max files to list in [STATUS]
const STATUS_MAX_FILES: usize = 10;

/// File path patterns to exclude from diff and status (noise, not signal).
const SKIP_PATTERNS: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "/target/",
    "/node_modules/",
    "/dist/",
    "/build/",
    "/.next/",
    "/__pycache__/",
];

pub struct PreparedInput {
    pub active_repo: PathBuf,
    pub repo_id: String,
    pub branch: String,
    pub commit_sha: String,
    pub files_touched: Vec<String>,
    pub prompt: String,
}

/// Top-level entry. Picks the active repo and prepares the prompt.
/// Returns None if no suitable repo found.
pub async fn prepare(mode: &Mode) -> Option<PreparedInput> {
    let repos = detect_repos(mode).await;
    if repos.is_empty() {
        debug!("no git repos detected — cannot prepare summary input");
        return None;
    }

    let active = pick_active_repo(&repos, mode).await?;

    // Resolve branch name. Detached HEAD / Unborn are not summarizable.
    let branch_name = match &active.branch {
        BranchState::Named(name) => name.clone(),
        BranchState::Detached(marker) => {
            info!(
                repo_id = %active.repo_id,
                root = %active.root.display(),
                marker = %marker,
                "cannot prepare summary: active repo is in detached HEAD"
            );
            return None;
        }
        BranchState::Unborn => {
            info!(
                root = %active.root.display(),
                "cannot prepare summary: active repo is unborn"
            );
            return None;
        }
    };

    debug!(
        repo_id = %active.repo_id,
        root = %active.root.display(),
        branch = %branch_name,
        "selected active repo for summary"
    );

    let last_commit = git::last_commit_subject(&active.root).await.unwrap_or_default();
    let status = git::status_porcelain(&active.root).await.unwrap_or_default();
    let commit_sha = git::current_head(&active.root).await.unwrap_or_default();
    let diff = git::working_diff(&active.root).await.unwrap_or_default();

    // If working tree is clean, fall back to last commit's diff
    let diff = if diff.trim().is_empty() {
        match git::run_diff_for_last_commit(&active.root).await {
            Ok(d) => d,
            Err(_) => String::new(),
        }
    } else {
        diff
    };

    let status_clean = filter_status(&status);
    let (diff_clean, files_touched) = filter_truncate_and_extract(&diff);

    let user_content = format!(
        "[LAST COMMIT]: {}\n[STATUS]: {}\n[DIFF]: {}",
        last_commit.trim(),
        status_clean.trim(),
        diff_clean.trim()
    );

    let prompt = format!(
        "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
        SYSTEM_PROMPT, user_content
    );

    Some(PreparedInput {
        active_repo: active.root.clone(),
        repo_id: active.repo_id.clone(),
        branch: branch_name,
        commit_sha,
        files_touched,
        prompt,
    })
}

/// Pick the "active" repo using:
///   c) IDE workspace match (VS Code / Cursor / IntelliJ paths) — preferred
///   a) Most recently modified (mtime of repo root) — fallback
async fn pick_active_repo<'a>(
    repos: &'a [DetectedRepo],
    mode: &Mode,
) -> Option<&'a DetectedRepo> {
    // Strategy C: check if any repo matches a captured IDE workspace
    let ide_paths = collect_ide_paths(mode);
    for ide_path in &ide_paths {
        for repo in repos {
            if ide_path.starts_with(&repo.root) {
                debug!(
                    repo_id = %repo.repo_id,
                    root = %repo.root.display(),
                    "active repo from IDE workspace match"
                );
                return Some(repo);
            }
        }
    }

    // Strategy A: most recently modified repo (mtime of repo root)
    let mut best: Option<(&'a DetectedRepo, SystemTime)> = None;
    for repo in repos {
        if let Ok(meta) = tokio::fs::metadata(&repo.root).await {
            if let Ok(mtime) = meta.modified() {
                match &best {
                    None => best = Some((repo, mtime)),
                    Some((_, prev)) if mtime > *prev => {
                        best = Some((repo, mtime));
                    }
                    _ => {}
                }
            }
        }
    }
    best.map(|(r, _)| r)
}

fn collect_ide_paths(mode: &Mode) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let ide_bundles = [
        "com.microsoft.VSCode",
        "com.todesktop.230313mzl4w4u92",  // Cursor
        "com.jetbrains.intellij",
    ];
    for bid in ide_bundles {
        if let Some(state) = mode.apps.state.get(bid) {
            if let Some(arr) = state.get("workspaces").and_then(|v| v.as_array()) {
                for w in arr {
                    if let Some(p) = w.as_str() {
                        out.push(PathBuf::from(p));
                    }
                }
            }
            if let Some(arr) = state.get("projects").and_then(|v| v.as_array()) {
                for p in arr {
                    if let Some(s) = p.as_str() {
                        out.push(PathBuf::from(s));
                    }
                }
            }
        }
    }
    out
}

/// Drop noise lines, cap at STATUS_MAX_FILES.
fn filter_status(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !is_skip_path(l))
        .collect();

    if lines.len() <= STATUS_MAX_FILES {
        lines.join("\n")
    } else {
        let extra = lines.len() - STATUS_MAX_FILES;
        let mut out = lines[..STATUS_MAX_FILES].join("\n");
        out.push_str(&format!("\n... {} more files", extra));
        out
    }
}

/// Filter skip-patterns and truncate to DIFF_CHAR_BUDGET.
/// Preserves diff structure — cuts at file boundaries when possible.
/// Filter skip-patterns, truncate to DIFF_CHAR_BUDGET, and also return
/// the list of file paths that survived filtering (for downstream
/// retrieval keying). Preserves diff structure — cuts at file boundaries
/// when possible.
fn filter_truncate_and_extract(raw: &str) -> (String, Vec<String>) {
    if raw.is_empty() {
        return (String::new(), Vec::new());
    }

    let blocks = split_diff_by_file(raw);

    let mut kept: Vec<&str> = Vec::new();
    let mut skipped = 0;
    for block in &blocks {
        if is_skip_path(block) {
            skipped += 1;
            continue;
        }
        kept.push(block);
    }

    if skipped > 0 {
        debug!(skipped, "diff blocks filtered out");
    }

    // Extract file paths from the kept blocks (signal-bearing files only).
    let files_touched: Vec<String> = kept.iter()
        .filter_map(|block| extract_file_path(block))
        .collect();

    let mut out = String::new();
    for block in &kept {
        if out.len() + block.len() <= DIFF_CHAR_BUDGET {
            out.push_str(block);
        } else {
            let remaining = DIFF_CHAR_BUDGET.saturating_sub(out.len());
            if remaining > 200 {
                let cutoff = block.char_indices()
                    .take_while(|(i, _)| *i < remaining.saturating_sub(50))
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                out.push_str(&block[..cutoff]);
                out.push_str("\n... [diff truncated] ...\n");
            }
            break;
        }
    }

    (out, files_touched)
}

/// Pull the `b/PATH` filename from the first line of a diff block.
/// Returns None if the block doesn't start with a parseable `diff --git`.
fn extract_file_path(block: &str) -> Option<String> {
    let first_line = block.lines().next()?;
    // "diff --git a/PATH b/PATH"
    let after_b = first_line.split(" b/").nth(1)?;
    let path = after_b.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn split_diff_by_file(raw: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut last_idx = 0;
    let mut first = true;

    for (idx, _) in raw.match_indices("diff --git ") {
        if first {
            last_idx = idx;
            first = false;
            continue;
        }
        out.push(&raw[last_idx..idx]);
        last_idx = idx;
    }

    if last_idx < raw.len() {
        out.push(&raw[last_idx..]);
    }

    if out.is_empty() && !raw.is_empty() {
        out.push(raw);
    }

    out
}

fn is_skip_path(s: &str) -> bool {
    SKIP_PATTERNS.iter().any(|p| s.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_patterns_match() {
        assert!(is_skip_path(" M Cargo.lock"));
        assert!(is_skip_path("diff --git a/target/foo.o b/target/foo.o"));
        assert!(is_skip_path("?? node_modules/foo/bar.js"));
        assert!(!is_skip_path(" M src/main.rs"));
    }

    #[test]
    fn split_diff_basic() {
        let diff = "diff --git a/foo.rs b/foo.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/bar.rs b/bar.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let blocks = split_diff_by_file(diff);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("diff --git a/foo.rs"));
        assert!(blocks[1].starts_with("diff --git a/bar.rs"));
    }

    #[test]
    fn filter_status_caps() {
        let raw = (0..15).map(|i| format!(" M file{}.rs", i)).collect::<Vec<_>>().join("\n");
        let out = filter_status(&raw);
        assert!(out.contains("5 more files"));
    }

    #[test]
    fn extract_file_paths_from_diff() {
        let diff = "diff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/README.md b/README.md\n@@ -1 +1 @@\n-x\n+y\n";
        let (_, files) = filter_truncate_and_extract(diff);
        assert_eq!(files, vec!["src/main.rs".to_string(), "README.md".to_string()]);
    }
}
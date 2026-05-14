//! Detect git repositories from paths captured in the work mode.
//!
//! Sources of paths:
//!   - Terminal handler state: window CWDs
//!   - VS Code/Cursor handler state: workspaces
//!   - IntelliJ handler state: projects
//!
//! For each path, walk up to find a .git directory. Each detected repo
//! carries its identity (repo_id) and branch state, computed exactly
//! once at detection time and passed downstream as DetectedRepo.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::modes::Mode;

use super::git::{current_branch, find_repo_root, repo_id, BranchState};

/// A detected git repository with its identity and current branch.
///
/// Computed once during detection; downstream consumers should not
/// re-shell to git for this information.
#[derive(Debug, Clone)]
pub struct DetectedRepo {
    /// Filesystem path to the repo root
    pub root: PathBuf,
    /// Stable identifier: SHA of the repo's first commit
    pub repo_id: String,
    /// Current branch state (Named / Detached / Unborn)
    pub branch: BranchState,
}

/// Extract candidate paths from a Mode's per-app state and return
/// detected git repos with identity and branch state.
///
/// Repos with no commits (Unborn) are filtered out at detection — they
/// have no stable identifier and no meaningful state to summarize.
/// Detached HEAD repos ARE returned so callers can log/skip with context.
pub async fn detect_repos(mode: &Mode) -> Vec<DetectedRepo> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    for (bundle_id, state) in &mode.apps.state {
        match bundle_id.as_str() {
            "com.apple.Terminal" => {
                if let Some(windows) = state.get("windows").and_then(|v| v.as_array()) {
                    for w in windows {
                        if let Some(cwd) = w.get("cwd").and_then(|v| v.as_str()) {
                            candidates.push(PathBuf::from(cwd));
                        }
                    }
                }
            }
            "com.microsoft.VSCode" | "com.todesktop.230313mzl4w4u92" => {
                if let Some(arr) = state.get("workspaces").and_then(|v| v.as_array()) {
                    for w in arr {
                        if let Some(p) = w.as_str() {
                            candidates.push(PathBuf::from(p));
                        }
                    }
                }
            }
            "com.jetbrains.intellij" => {
                if let Some(arr) = state.get("projects").and_then(|v| v.as_array()) {
                    for p in arr {
                        if let Some(s) = p.as_str() {
                            candidates.push(PathBuf::from(s));
                        }
                    }
                }
            }
            _ => {}  // other handlers don't expose paths
        }
    }

    // Walk up each candidate, dedupe by repo root, then enrich with
    // repo_id and branch state. Skip Unborn repos.
    let mut seen_roots: HashSet<PathBuf> = HashSet::new();
    let mut detected = Vec::new();

    for path in candidates {
        let root = match find_repo_root(&path) {
            Some(r) => r,
            None => continue,
        };

        if !seen_roots.insert(root.clone()) {
            continue;
        }

        let id = match repo_id(&root).await {
            Some(id) => id,
            None => {
                tracing::info!(
                    root = %root.display(),
                    "repo has no commits yet, skipping"
                );
                continue;
            }
        };

        let branch = current_branch(&root).await;

        detected.push(DetectedRepo {
            root,
            repo_id: id,
            branch,
        });
    }

    detected
}
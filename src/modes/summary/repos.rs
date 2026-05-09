//! Detect git repositories from paths captured in the work mode.
//!
//! Sources of paths:
//!   - Terminal handler state: window CWDs
//!   - VS Code/Cursor handler state: workspaces
//!   - IntelliJ handler state: projects
//!
//! For each path, walk up to find a .git directory. Dedupe by repo root.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::modes::Mode;

use super::git::find_repo_root;

/// Extract candidate paths from a Mode's per-app state and return unique
/// git repo roots.
pub async fn detect_repos(mode: &Mode) -> Vec<PathBuf> {
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

    // Walk up each candidate to find its repo root, dedupe.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut roots = Vec::new();
    for path in candidates {
        if let Some(root) = find_repo_root(&path) {
            if seen.insert(root.clone()) {
                roots.push(root);
            }
        }
    }

    roots
}
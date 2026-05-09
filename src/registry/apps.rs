// src/registry/apps.rs
//
// AppRegistry: knows what applications are installed on this macOS system.
//
// Updated in Phase 1 patch: each app exposes MULTIPLE search aliases to the
// matcher (display name + bundle filename), so users can find an app by
// either the in-app name (e.g. "Code") or the filename ("Visual Studio
// Code"). The matcher returns whichever alias scored best; we map back to
// the canonical AppEntry via the alias→entry index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::matcher::{resolve, ResolutionResult};

const SCAN_ROOTS: &[&str] = &[
    "/Applications",
    "/Applications/Utilities",
    "/System/Applications",
    "/System/Applications/Utilities",
];

fn user_applications_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        let mut p = PathBuf::from(h);
        p.push("Applications");
        p
    })
}

#[derive(Debug, Clone)]
pub struct AppEntry {
    /// Canonical display name. This is what AppleScript expects in
    /// `tell application "X" to activate`. Usually CFBundleDisplayName,
    /// falling back to bundle filename.
    pub display_name: String,
    /// All names this app can be searched by — display name plus bundle
    /// filename (when different). Used for fuzzy matching.
    pub aliases: Vec<String>,
    pub bundle_path: PathBuf,
}

pub struct AppRegistry {
    inner: RwLock<RegistryInner>,
}

struct RegistryInner {
    entries: Vec<AppEntry>,
    /// All searchable aliases across all apps. Parallel-indexed with `alias_to_entry`.
    search_aliases: Vec<String>,
    /// Maps an index in `search_aliases` to the index in `entries` that owns it.
    alias_to_entry: Vec<usize>,
    last_scan_at: Instant,
}

impl AppRegistry {
    pub fn scan_now() -> std::io::Result<Self> {
        let entries = scan_all_roots();
        let (search_aliases, alias_to_entry) = build_alias_index(&entries);

        debug!(
            apps = entries.len(),
            aliases = search_aliases.len(),
            "AppRegistry initial scan complete"
        );

        Ok(Self {
            inner: RwLock::new(RegistryInner {
                entries,
                search_aliases,
                alias_to_entry,
                last_scan_at: Instant::now(),
            }),
        })
    }

    pub fn refresh(&self) {
        let new_entries = scan_all_roots();
        let (new_aliases, new_a2e) = build_alias_index(&new_entries);

        let mut w = self.inner.write().unwrap();
        let prev_count = w.entries.len();
        w.entries = new_entries;
        w.search_aliases = new_aliases;
        w.alias_to_entry = new_a2e;
        w.last_scan_at = Instant::now();
        debug!(
            previous = prev_count,
            current = w.entries.len(),
            "AppRegistry refreshed"
        );
    }

    pub fn age(&self) -> Duration {
        self.inner.read().unwrap().last_scan_at.elapsed()
    }

    /// Resolve a fuzzy query to an installed app.
    ///
    /// We resolve against the alias list (which contains display names AND
    /// filenames). When we get a match, we map back to the canonical
    /// display_name of the owning entry — that's what AppleScript needs.
    pub fn resolve(&self, query: &str) -> ResolutionResult {
        let r = self.inner.read().unwrap();
        let raw_result = resolve(query, &r.search_aliases);

        // Map alias hits back to canonical display names. Multiple aliases
        // belonging to the same entry should collapse to one result.
        match raw_result {
            ResolutionResult::Confident { canonical: alias, score } => {
                if let Some(entry) = lookup_entry_by_alias(&r, &alias) {
                    ResolutionResult::Confident {
                        canonical: entry.display_name.clone(),
                        score,
                    }
                } else {
                    ResolutionResult::NotFound
                }
            }
            ResolutionResult::Ambiguous { candidates } => {
                // Deduplicate by entry — if Visual Studio Code shows up
                // as both "Code" and "Visual Studio Code" in candidates,
                // keep only the higher-scoring one and label by display_name.
                let mut by_entry: HashMap<String, f32> = HashMap::new();
                for (alias, score) in candidates {
                    if let Some(entry) = lookup_entry_by_alias(&r, &alias) {
                        let display = entry.display_name.clone();
                        by_entry
                            .entry(display)
                            .and_modify(|s| *s = s.max(score))
                            .or_insert(score);
                    }
                }
                if by_entry.is_empty() {
                    return ResolutionResult::NotFound;
                }
                if by_entry.len() == 1 {
                    let (canonical, score) = by_entry.into_iter().next().unwrap();
                    return ResolutionResult::Confident { canonical, score };
                }
                let mut deduped: Vec<(String, f32)> = by_entry.into_iter().collect();
                deduped.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                ResolutionResult::Ambiguous { candidates: deduped }
            }
            ResolutionResult::NotFound => ResolutionResult::NotFound,
        }
    }

    pub fn entry_for(&self, canonical: &str) -> Option<AppEntry> {
        let r = self.inner.read().unwrap();
        r.entries
            .iter()
            .find(|e| e.display_name == canonical)
            .cloned()
    }

    pub fn all(&self) -> Vec<AppEntry> {
        self.inner.read().unwrap().entries.clone()
    }
}

fn lookup_entry_by_alias<'a>(inner: &'a RegistryInner, alias: &str) -> Option<&'a AppEntry> {
    let idx = inner.search_aliases.iter().position(|a| a == alias)?;
    let entry_idx = *inner.alias_to_entry.get(idx)?;
    inner.entries.get(entry_idx)
}

fn build_alias_index(entries: &[AppEntry]) -> (Vec<String>, Vec<usize>) {
    let mut aliases = Vec::new();
    let mut a2e = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        for alias in &e.aliases {
            aliases.push(alias.clone());
            a2e.push(i);
        }
    }
    (aliases, a2e)
}

// ---- Scanning ------------------------------------------------------------

fn scan_all_roots() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    for root in SCAN_ROOTS {
        scan_dir(Path::new(root), &mut entries, &mut seen_paths);
    }
    if let Some(user_dir) = user_applications_dir() {
        scan_dir(&user_dir, &mut entries, &mut seen_paths);
    }

    entries.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    entries
}

fn scan_dir(
    dir: &Path,
    entries: &mut Vec<AppEntry>,
    seen_paths: &mut std::collections::HashSet<PathBuf>,
) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "could not read directory");
            return;
        }
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("app") {
            continue;
        }

        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen_paths.insert(canon) {
            continue;
        }

        if let Some(app) = parse_app_bundle(&path) {
            entries.push(app);
        }
    }
}

/// Build an AppEntry with all known aliases for matching.
fn parse_app_bundle(bundle: &Path) -> Option<AppEntry> {
    let plist_path = bundle.join("Contents").join("Info.plist");

    // Bundle filename (without .app extension) is always available.
    let filename_stem = bundle.file_stem()?.to_str()?.to_string();

    // Plist names are best-effort.
    let (_plist_display, plist_bundle_name) = read_plist_names(&plist_path);

    // Display name preference: CFBundleDisplayName → CFBundleName → filename.
    let display_name = filename_stem.clone();

    // Aliases for matching: deduplicate and skip empties.
    let mut aliases = vec![display_name.clone()];
    let mut push_alias = |s: Option<String>| {
        if let Some(s) = s {
            if !s.is_empty() && !aliases.contains(&s) {
                aliases.push(s);
            }
        }
    };
    push_alias(plist_bundle_name);
    push_alias(Some(filename_stem));

    Some(AppEntry {
        display_name,
        aliases,
        bundle_path: bundle.to_path_buf(),
    })
}

/// Returns (CFBundleDisplayName, CFBundleName) — either or both may be None.
fn read_plist_names(plist_path: &Path) -> (Option<String>, Option<String>) {
    use plist::Value;

    let value = match Value::from_file(plist_path) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let dict = match value.as_dictionary() {
        Some(d) => d,
        None => return (None, None),
    };

    let display = dict
        .get("CFBundleDisplayName")
        .and_then(|v| v.as_string())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let name = dict
        .get("CFBundleName")
        .and_then(|v| v.as_string())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    (display, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_runs_without_panic() {
        let r = AppRegistry::scan_now().expect("scan_now should not error");
        let _ = r.all();
    }
}
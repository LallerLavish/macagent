//! IntelliJ IDEA state handler.
//!
//! Capture: reads JetBrains' recentProjects.xml from the latest installed
//! IntelliJ version directory. Correlates with current window count via
//! AppleScript to determine which projects are actually open.
//!
//! Restore: launches IntelliJ via `open -na "IntelliJ IDEA" --args <path>`.
//! No `idea` CLI required.

use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use super::{AppStateHandler, HandlerError, Role};

const BUNDLE_ID: &str = "com.jetbrains.intellij";
const NAME: &str = "IntelliJ IDEA";
const APP_NAME: &str = "IntelliJ IDEA";

const APPLESCRIPT_TIMEOUT: Duration = Duration::from_secs(5);
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(5);
const RESTORE_INTER_PROJECT_DELAY: Duration = Duration::from_millis(800);

#[derive(Debug, Serialize, Deserialize)]
struct IntellijState {
    projects: Vec<String>,
}

#[derive(Debug)]
struct RecentEntry {
    path: String,
    timestamp: i64,
}

pub struct IntellijHandler;

impl IntellijHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppStateHandler for IntellijHandler {
    fn bundle_id(&self) -> &str {
        BUNDLE_ID
    }

    fn name(&self) -> &str {
        NAME
    }

    fn role(&self) -> Role {
        Role::Ide
    }

    async fn capture(&self) -> Result<Option<Value>, HandlerError> {
        let window_count = count_intellij_windows().await?;
        if window_count == 0 {
            debug!("IntelliJ has no windows");
            return Ok(None);
        }

        let xml_path = match find_recent_projects_xml() {
            Some(p) => p,
            None => {
                debug!("IntelliJ recentProjects.xml not found");
                return Ok(None);
            }
        };

        let xml_content = tokio::fs::read_to_string(&xml_path).await?;
        let mut entries = parse_recent_projects(&xml_content)?;

        // Sort by timestamp descending — most recent first
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        if entries.is_empty() {
            debug!("IntelliJ has no recent projects");
            return Ok(None);
        }

        let take = window_count.min(entries.len());
        if window_count > entries.len() {
            warn!(
                windows = window_count,
                recents = entries.len(),
                "more IntelliJ windows than recent projects — some won't be captured"
            );
        }

        let projects: Vec<String> = entries
            .into_iter()
            .take(take)
            .map(|e| expand_user_home(&e.path))
            .collect();

        debug!(count = projects.len(), "captured IntelliJ projects");

        let state = IntellijState { projects };
        Ok(Some(serde_json::to_value(state)?))
    }

    async fn restore(&self, state: &Value) -> Result<(), HandlerError> {
        let parsed: IntellijState = serde_json::from_value(state.clone())
            .map_err(|e| HandlerError::InvalidState(e.to_string()))?;

        if parsed.projects.is_empty() {
            return Ok(());
        }

        for path in &parsed.projects {
            if let Err(e) = open_project(path).await {
                warn!(path = %path, error = %e, "failed to open IntelliJ project");
            }
            // IntelliJ takes longer than VS Code to spawn — give it room.
            sleep(RESTORE_INTER_PROJECT_DELAY).await;
        }

        Ok(())
    }
}

// ---- Window counting via AppleScript -----------------------------------

const COUNT_WINDOWS_SCRIPT: &str = r#"
tell application "System Events"
    if exists (process "idea") then
        tell process "idea"
            return count of windows
        end tell
    else
        return 0
    end if
end tell
"#;

async fn count_intellij_windows() -> Result<usize, HandlerError> {
    let stdout = run_applescript(COUNT_WINDOWS_SCRIPT).await?;
    let count: usize = stdout
        .trim()
        .parse()
        .map_err(|_| HandlerError::AppleScript(format!("unexpected window count: {stdout}")))?;
    debug!(windows = count, "IntelliJ window count");
    Ok(count)
}

// ---- recentProjects.xml location ---------------------------------------

/// Find the latest IntelliJIdea<version>/options/recentProjects.xml.
/// Returns None if no IntelliJ version directory exists.
fn find_recent_projects_xml() -> Option<PathBuf> {
    let base = dirs::config_dir()?.join("JetBrains");
    if !base.exists() {
        return None;
    }

    let entries = std::fs::read_dir(&base).ok()?;
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.starts_with("IntelliJIdea"))
                .unwrap_or(false)
        })
        .collect();

    if versions.is_empty() {
        return None;
    }

    // String-sort works because the format is IntelliJIdeaYYYY.M (e.g.,
    // IntelliJIdea2026.1). Latest version sorts last.
    versions.sort();
    let latest = versions.pop()?;
    let xml = latest.join("options").join("recentProjects.xml");
    if xml.exists() {
        Some(xml)
    } else {
        None
    }
}

// ---- XML parsing -------------------------------------------------------

/// Parse recentProjects.xml. Extracts each `<entry key="..." />` along with
/// its child `<option name="projectOpenTimestamp" value="..." />`.
fn parse_recent_projects(xml: &str) -> Result<Vec<RecentEntry>, HandlerError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries: Vec<RecentEntry> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_timestamp: i64 = 0;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let name_bytes = name.as_ref();

                if name_bytes == b"entry" {
                    // New entry — extract `key` attribute as the project path.
                    if let Some(path) = extract_attr(&e, "key") {
                        // If we were tracking a previous entry without finalizing,
                        // push it now (defensive — shouldn't normally happen).
                        if let Some(prev_path) = current_path.take() {
                            entries.push(RecentEntry {
                                path: prev_path,
                                timestamp: current_timestamp,
                            });
                        }
                        current_path = Some(path);
                        current_timestamp = 0;
                    }
                } else if name_bytes == b"option" {
                    // Look for projectOpenTimestamp inside the current entry.
                    if current_path.is_some() {
                        let name_attr = extract_attr(&e, "name");
                        let value_attr = extract_attr(&e, "value");
                        if name_attr.as_deref() == Some("projectOpenTimestamp") {
                            if let Some(v) = value_attr {
                                current_timestamp = v.parse().unwrap_or(0);
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"entry" {
                    if let Some(path) = current_path.take() {
                        entries.push(RecentEntry {
                            path,
                            timestamp: current_timestamp,
                        });
                        current_timestamp = 0;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(HandlerError::InvalidState(format!("XML parse error: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(entries)
}

/// Extract a named attribute from a quick-xml start/empty tag.
fn extract_attr(e: &quick_xml::events::BytesStart, key: &str) -> Option<String> {
    e.attributes().find_map(|attr| {
        let attr = attr.ok()?;
        if attr.key.as_ref() == key.as_bytes() {
            Some(String::from_utf8_lossy(&attr.value).into_owned())
        } else {
            None
        }
    })
}

/// Replace `$USER_HOME$` with the actual home dir.
fn expand_user_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("$USER_HOME$") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), rest);
        }
    }
    path.to_string()
}

// ---- Restore -----------------------------------------------------------

async fn open_project(path: &str) -> Result<(), HandlerError> {
    let output = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("open")
            .arg("-na")
            .arg(APP_NAME)
            .arg("--args")
            .arg(path)
            .output(),
    )
    .await
    .map_err(|_| HandlerError::Subprocess(format!("open IntelliJ for {path} timed out")))?
    .map_err(|e| HandlerError::Subprocess(format!("open spawn failed: {e}")))?;

    if !output.status.success() {
        return Err(HandlerError::Subprocess(format!(
            "open failed for {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

// ---- AppleScript runner -----------------------------------------------

async fn run_applescript(script: &str) -> Result<String, HandlerError> {
    let output = timeout(
        APPLESCRIPT_TIMEOUT,
        Command::new("osascript").arg("-e").arg(script).output(),
    )
    .await
    .map_err(|_| HandlerError::AppleScript("osascript timed out".to_string()))?
    .map_err(|e| HandlerError::AppleScript(format!("osascript spawn failed: {e}")))?;

    if !output.status.success() {
        return Err(HandlerError::AppleScript(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_entry() {
        let xml = r#"<application>
  <component name="RecentProjectsManager">
    <option name="additionalInfo">
      <map>
        <entry key="$USER_HOME$/code/project1">
          <value>
            <RecentProjectMetaInfo>
              <option name="projectOpenTimestamp" value="1777182856564" />
            </RecentProjectMetaInfo>
          </value>
        </entry>
      </map>
    </option>
  </component>
</application>"#;

        let entries = parse_recent_projects(xml).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "$USER_HOME$/code/project1");
        assert_eq!(entries[0].timestamp, 1777182856564);
    }

    #[test]
    fn parse_multiple_entries_sorted_by_timestamp() {
        let xml = r#"<application>
  <component name="RecentProjectsManager">
    <option name="additionalInfo">
      <map>
        <entry key="$USER_HOME$/code/old">
          <value>
            <RecentProjectMetaInfo>
              <option name="projectOpenTimestamp" value="1000000" />
            </RecentProjectMetaInfo>
          </value>
        </entry>
        <entry key="$USER_HOME$/code/new">
          <value>
            <RecentProjectMetaInfo>
              <option name="projectOpenTimestamp" value="2000000" />
            </RecentProjectMetaInfo>
          </value>
        </entry>
      </map>
    </option>
  </component>
</application>"#;

        let mut entries = parse_recent_projects(xml).unwrap();
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "$USER_HOME$/code/new");
        assert_eq!(entries[1].path, "$USER_HOME$/code/old");
    }

    #[test]
    fn expand_user_home_works() {
        let home = dirs::home_dir().unwrap();
        let expanded = expand_user_home("$USER_HOME$/code/project");
        assert!(expanded.starts_with(&format!("{}", home.display())));
        assert!(expanded.ends_with("/code/project"));
    }

    #[test]
    fn expand_user_home_passthrough() {
        assert_eq!(expand_user_home("/absolute/path"), "/absolute/path");
    }

    #[test]
    fn empty_xml_returns_no_entries() {
        let xml = r#"<application></application>"#;
        let entries = parse_recent_projects(xml).unwrap();
        assert!(entries.is_empty());
    }
}
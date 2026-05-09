use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

use crate::modes::error::ModeError;

#[derive(Debug, Clone)]
pub struct RunningApp {
    pub bundle_id: String,
    pub name: String,
    pub pid: i32,
}

/// Bundle IDs we never report as "running user apps" — they should never
/// appear in mode launch lists or close lists.
const ALWAYS_EXCLUDE: &[&str] = &[
    "com.apple.finder",          // closing Finder breaks macOS UX
    "com.apple.systemuiserver",  // system service
    "com.apple.dock",            // system service
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
];

/// AppleScript that returns one app per line: "<bundle_id>|<name>|<pid>"
const ENUMERATE_SCRIPT: &str = r#"
set output to ""
tell application "System Events"
    repeat with p in (every process whose background only is false)
        try
            set bid to bundle identifier of p
            set nm to name of p
            set pidv to unix id of p
            set output to output & bid & "|" & nm & "|" & pidv & linefeed
        end try
    end repeat
end tell
return output
"#;

const APPLESCRIPT_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn running_user_apps() -> Result<Vec<RunningApp>, ModeError> {
    let output = timeout(
        APPLESCRIPT_TIMEOUT,
        Command::new("osascript")
            .arg("-e")
            .arg(ENUMERATE_SCRIPT)
            .output(),
    )
    .await
    .map_err(|_| ModeError::Capture("osascript enumerate timed out".into()))?
    .map_err(|e| ModeError::Capture(format!("osascript spawn failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ModeError::Capture(format!("osascript failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut apps = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() != 3 {
            warn!(line = %line, "skipping malformed app enumeration line");
            continue;
        }
        let bundle_id = parts[0].trim().to_string();
        let name = parts[1].trim().to_string();
        let pid = match parts[2].trim().parse::<i32>() {
            Ok(p) => p,
            Err(_) => {
                warn!(line = %line, "bad pid, skipping");
                continue;
            }
        };

        if bundle_id.is_empty() {
            // Some processes (e.g., scripts) have no bundle id — skip
            continue;
        }
        if ALWAYS_EXCLUDE.contains(&bundle_id.as_str()) {
            continue;
        }
        if bundle_id.contains("macagent") {
            continue;
        }

        apps.push(RunningApp { bundle_id, name, pid });
    }

    Ok(apps)
}
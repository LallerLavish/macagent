use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::modes::capture::apps::RunningApp;
use crate::modes::error::ModeError;

const CLOSE_TIMEOUT: Duration = Duration::from_secs(8);

/// Bundle IDs whose unsaved state we cannot reliably detect in M1.
/// These trigger the dirty-doc prompt before close. M2 replaces this with
/// per-app handlers that check actual document state.
pub const DOCUMENT_BASED_APPS: &[&str] = &[
    "com.apple.TextEdit",
    "com.apple.iWork.Pages",
    "com.apple.iWork.Numbers",
    "com.apple.iWork.Keynote",
    "com.microsoft.Word",
    "com.microsoft.Excel",
    "com.microsoft.Powerpoint",
    "com.microsoft.VSCode",
    "com.todesktop.230313mzl4w4u92",  // Cursor
    "com.apple.Notes",
    "com.apple.dt.Xcode",
    "com.figma.Desktop",
    "com.tinyspeck.slackmacgap",      // Slack drafts
];

pub fn is_document_based(bundle_id: &str) -> bool {
    DOCUMENT_BASED_APPS.contains(&bundle_id)
}

/// Close apps sequentially. Sequential because we want predictable error
/// reporting and to avoid AppleScript contention.
pub async fn close_apps(apps: &[RunningApp]) -> Result<Vec<String>, ModeError> {
    let mut closed = Vec::new();
    for app in apps {
        match close_one(&app.bundle_id).await {
            Ok(()) => {
                debug!(bundle_id = %app.bundle_id, name = %app.name, "closed");
                closed.push(app.bundle_id.clone());
            }
            Err(e) => {
                warn!(
                    bundle_id = %app.bundle_id,
                    name = %app.name,
                    error = %e,
                    "close failed, continuing"
                );
            }
        }
    }
    Ok(closed)
}

async fn close_one(bundle_id: &str) -> Result<(), ModeError> {
    // AppleScript "tell application id ... to quit" is gentler than SIGTERM.
    // It lets the app save state and respect quit handlers.
    let script = format!(r#"tell application id "{}" to quit"#, bundle_id);

    let output = timeout(
        CLOSE_TIMEOUT,
        Command::new("osascript").arg("-e").arg(&script).output(),
    )
    .await
    .map_err(|_| ModeError::Restore(format!("quit of {bundle_id} timed out")))?
    .map_err(|e| ModeError::Restore(format!("osascript spawn failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ModeError::Restore(format!("quit failed for {bundle_id}: {stderr}")));
    }
    Ok(())
}
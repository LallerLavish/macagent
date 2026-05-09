//! VS Code state handler.
//!
//! Reads VS Code's storage.json which contains windowsState — both the
//! last active window and any additional open windows. This is the most
//! reliable source for "what's currently open" because VS Code writes to
//! it on quit and during normal operation.
//!
//! Restore: invokes `code <path>` for the first folder (reuses default
//! launch window), `code -n <path>` for subsequent ones (forces new window).
//! Falls back to `open -a "Visual Studio Code" -n --args <path>` if the
//! `code` CLI isn't installed.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use super::{AppStateHandler, HandlerError, Role};

const BUNDLE_ID: &str = "com.microsoft.VSCode";
const NAME: &str = "Visual Studio Code";
const APP_DIR_NAME: &str = "Code";  // ~/Library/Application Support/Code

const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(5);
const RESTORE_INTER_WINDOW_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Serialize, Deserialize)]
struct VsCodeState {
    workspaces: Vec<String>,
}

pub struct VsCodeHandler;

impl VsCodeHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppStateHandler for VsCodeHandler {
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
        let workspaces = read_open_workspaces(APP_DIR_NAME).await?;
        if workspaces.is_empty() {
            debug!("VS Code has no open workspaces");
            return Ok(None);
        }

        debug!(count = workspaces.len(), "captured VS Code workspaces");
        let state = VsCodeState { workspaces };
        Ok(Some(serde_json::to_value(state)?))
    }

    async fn restore(&self, state: &Value) -> Result<(), HandlerError> {
        let parsed: VsCodeState = serde_json::from_value(state.clone())
            .map_err(|e| HandlerError::InvalidState(e.to_string()))?;

        if parsed.workspaces.is_empty() {
            return Ok(());
        }

        let cli_available = which_cli("code").await;
        debug!(cli_available, "VS Code restore strategy");

        for (i, path) in parsed.workspaces.iter().enumerate() {
            let result = if cli_available {
                open_via_cli("code", path, i == 0).await
            } else {
                open_via_open_command(NAME, path).await
            };

            if let Err(e) = result {
                warn!(path = %path, error = %e, "failed to open VS Code workspace");
            }

            if i + 1 < parsed.workspaces.len() {
                sleep(RESTORE_INTER_WINDOW_DELAY).await;
            }
        }

        Ok(())
    }
}

// ---- Shared logic (used by both VS Code and Cursor handlers) ----------

/// Read open workspace folder paths from a VS Code-family app's storage.json.
/// `app_dir_name` is the directory name under ~/Library/Application Support/
/// (e.g., "Code" for VS Code, "Cursor" for Cursor).
///
/// Returns deduplicated list with lastActiveWindow first, then any other
/// openedWindows folders.
pub(crate) async fn read_open_workspaces(app_dir_name: &str) -> Result<Vec<String>, HandlerError> {
    let path = match storage_json_path(app_dir_name) {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };

    if !path.exists() {
        debug!(?path, "storage.json not found");
        return Ok(Vec::new());
    }

    let content = tokio::fs::read_to_string(&path).await?;
    let json: Value = serde_json::from_str(&content)?;

    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. lastActiveWindow first — it's the foreground window
    if let Some(folder) = json
        .pointer("/windowsState/lastActiveWindow/folder")
        .and_then(|v| v.as_str())
    {
        if let Some(decoded) = uri_to_path(folder) {
            seen.insert(decoded.clone());
            out.push(decoded);
        }
    }

    // 2. openedWindows — any others, deduped against lastActiveWindow
    if let Some(arr) = json
        .pointer("/windowsState/openedWindows")
        .and_then(|v| v.as_array())
    {
        for entry in arr {
            if let Some(folder) = entry.get("folder").and_then(|v| v.as_str()) {
                if let Some(decoded) = uri_to_path(folder) {
                    if !seen.contains(&decoded) {
                        seen.insert(decoded.clone());
                        out.push(decoded);
                    }
                }
            }
        }
    }

    Ok(out)
}

fn storage_json_path(app_dir_name: &str) -> Option<PathBuf> {
    let mut p = dirs::config_dir()?;
    p.push(app_dir_name);
    p.push("User/globalStorage/storage.json");
    Some(p)
}

/// Convert "file:///some/path" → "/some/path" with percent-decoding.
fn uri_to_path(uri: &str) -> Option<String> {
    let stripped = uri.strip_prefix("file://")?;
    Some(percent_decode(stripped))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---- Subprocess helpers (also shared with cursor.rs) -------------------

pub(crate) async fn which_cli(name: &str) -> bool {
    timeout(
        Duration::from_secs(1),
        Command::new("which").arg(name).output(),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(|o| o.status.success())
    .unwrap_or(false)
}

pub(crate) async fn open_via_cli(
    cli: &str,
    path: &str,
    is_first: bool,
) -> Result<(), HandlerError> {
    let mut cmd = Command::new(cli);
    if !is_first {
        cmd.arg("-n");
    }
    cmd.arg(path);

    let output = timeout(SUBPROCESS_TIMEOUT, cmd.output())
        .await
        .map_err(|_| HandlerError::Subprocess(format!("{cli} {path} timed out")))?
        .map_err(|e| HandlerError::Subprocess(format!("{cli} spawn failed: {e}")))?;

    if !output.status.success() {
        return Err(HandlerError::Subprocess(format!(
            "{cli} failed for {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(crate) async fn open_via_open_command(app_name: &str, path: &str) -> Result<(), HandlerError> {
    let output = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("open")
            .arg("-a").arg(app_name)
            .arg("-n")
            .arg("--args").arg(path)
            .output(),
    )
    .await
    .map_err(|_| HandlerError::Subprocess(format!("open -a {app_name} timed out for {path}")))?
    .map_err(|e| HandlerError::Subprocess(format!("open spawn failed: {e}")))?;

    if !output.status.success() {
        return Err(HandlerError::Subprocess(format!(
            "open failed for {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

// ---- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_decodes_basic() {
        assert_eq!(
            uri_to_path("file:///Users/lavish/code/macagent"),
            Some("/Users/lavish/code/macagent".to_string())
        );
    }

    #[test]
    fn uri_decodes_spaces() {
        assert_eq!(
            uri_to_path("file:///Users/lavish/Data%20Structures"),
            Some("/Users/lavish/Data Structures".to_string())
        );
    }

    #[test]
    fn uri_rejects_non_file() {
        assert_eq!(uri_to_path("https://example.com"), None);
    }

    #[test]
    fn percent_decode_passthrough() {
        assert_eq!(percent_decode("/no/encoding"), "/no/encoding");
    }
}
//! Cursor state handler.
//!
//! Cursor is a VS Code fork — the storage format is identical, just at a
//! different path. This handler delegates to the shared logic in vscode.rs
//! with Cursor's app-specific paths.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

use super::vscode::{open_via_cli, open_via_open_command, read_open_workspaces, which_cli};
use super::{AppStateHandler, HandlerError, Role};

const BUNDLE_ID: &str = "com.todesktop.230313mzl4w4u92";
const NAME: &str = "Cursor";
const APP_DIR_NAME: &str = "Cursor";  // ~/Library/Application Support/Cursor

const RESTORE_INTER_WINDOW_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Serialize, Deserialize)]
struct CursorState {
    workspaces: Vec<String>,
}

pub struct CursorHandler;

impl CursorHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppStateHandler for CursorHandler {
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
            debug!("Cursor has no open workspaces");
            return Ok(None);
        }

        debug!(count = workspaces.len(), "captured Cursor workspaces");
        let state = CursorState { workspaces };
        Ok(Some(serde_json::to_value(state)?))
    }

    async fn restore(&self, state: &Value) -> Result<(), HandlerError> {
        let parsed: CursorState = serde_json::from_value(state.clone())
            .map_err(|e| HandlerError::InvalidState(e.to_string()))?;

        if parsed.workspaces.is_empty() {
            return Ok(());
        }

        let cli_available = which_cli("cursor").await;
        debug!(cli_available, "Cursor restore strategy");

        for (i, path) in parsed.workspaces.iter().enumerate() {
            let result = if cli_available {
                open_via_cli("cursor", path, i == 0).await
            } else {
                open_via_open_command(NAME, path).await
            };

            if let Err(e) = result {
                warn!(path = %path, error = %e, "failed to open Cursor workspace");
            }

            if i + 1 < parsed.workspaces.len() {
                sleep(RESTORE_INTER_WINDOW_DELAY).await;
            }
        }

        Ok(())
    }
}
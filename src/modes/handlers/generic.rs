//! Generic fallback handler.
//!
//! For any app without a specific handler. Capture returns None,
//! restore is a no-op. The launch step (in `restore::launch`) handles
//! actually opening the app via bundle ID — this handler doesn't even
//! need to know about that.

use async_trait::async_trait;
use serde_json::Value;

use super::{AppStateHandler, HandlerError, Role};

pub struct GenericHandler;

impl GenericHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppStateHandler for GenericHandler {
    fn bundle_id(&self) -> &str {
        "*"  // sentinel — registry never looks up by this
    }

    fn name(&self) -> &str {
        "Generic"
    }

    fn role(&self) -> Role {
        Role::Other
    }

    async fn capture(&self) -> Result<Option<Value>, HandlerError> {
        // Generic handler never captures state.
        Ok(None)
    }

    async fn restore(&self, _state: &Value) -> Result<(), HandlerError> {
        // No-op. Launch already happened via `open -b <bundle_id>`.
        Ok(())
    }
}
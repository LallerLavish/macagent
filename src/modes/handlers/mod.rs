pub mod generic;
pub mod terminal_app;
pub mod vscode;
pub mod chrome;
pub mod intellij;
pub mod cursor;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("AppleScript failed: {0}")]
    AppleScript(String),

    #[error("subprocess failed: {0}")]
    Subprocess(String),

    #[error("timeout waiting for app to be ready: {0}")]
    AppNotReady(String),

    #[error("invalid state data: {0}")]
    InvalidState(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// What role does an app play in a focus mode?
/// Used for future config overrides ("use iTerm2 as my terminal").
/// Not actively used in M2.1 but defined now to avoid retrofitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Terminal,
    Ide,
    Browser,
    Finder,
    Other,
}

#[async_trait]
pub trait AppStateHandler: Send + Sync {
    /// Bundle ID this handler manages. Must match exactly what `osascript`
    /// reports as the bundle identifier.
    fn bundle_id(&self) -> &str;

    /// Human-readable name. Used in logs and IPC responses.
    fn name(&self) -> &str;

    /// What role does this app play?
    fn role(&self) -> Role;

    /// Capture current state. Returns Ok(None) if the app is running but
    /// has no captureable state right now (e.g., no windows open).
    /// Returns Err only for genuine failures (AppleScript hung, etc).
    async fn capture(&self) -> Result<Option<serde_json::Value>, HandlerError>;

    /// Restore app to a previously captured state. Called after the app
    /// has been launched and is ready. Generic handler does nothing here.
    async fn restore(&self, state: &serde_json::Value) -> Result<(), HandlerError>;

    /// Does this app have unsaved work that would be lost on close?
    /// Default: false. Per-handler implementations override this in M2.4.
    async fn has_dirty_state(&self) -> Result<bool, HandlerError> {
        Ok(false)
    }
}

/// Registry mapping bundle IDs to their handlers, with a generic fallback.
pub struct HandlerRegistry {
    handlers: HashMap<String, Arc<dyn AppStateHandler>>,
    generic: Arc<dyn AppStateHandler>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            generic: Arc::new(generic::GenericHandler::new()),
        }
    }

    /// Register a handler. The handler's `bundle_id()` is used as the key.
    pub fn register(&mut self, handler: Arc<dyn AppStateHandler>) {
        let bid = handler.bundle_id().to_string();
        self.handlers.insert(bid, handler);
    }

    /// Get the handler for a bundle ID. Always returns *something* —
    /// the registered handler if one exists, else the generic fallback.
    pub fn for_bundle_id(&self, bundle_id: &str) -> Arc<dyn AppStateHandler> {
        self.handlers
            .get(bundle_id)
            .cloned()
            .unwrap_or_else(|| self.generic.clone())
    }

    /// True if a specific handler is registered for this bundle ID
    /// (not just the generic fallback). Useful for logging.
    pub fn has_specific(&self, bundle_id: &str) -> bool {
        self.handlers.contains_key(bundle_id)
    }

    /// Build a registry with all M2.1 handlers registered.
    /// As we add handlers in M2.2+, register them here.
    pub fn with_default_handlers() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(terminal_app::TerminalAppHandler::new()));
        r.register(Arc::new(vscode::VsCodeHandler::new())); 
        r.register(Arc::new(chrome::ChromeHandler::new())); 
        r.register(Arc::new(intellij::IntellijHandler::new()));
        r.register(Arc::new(cursor::CursorHandler::new())); 
        r
    }
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::with_default_handlers()
    }
}
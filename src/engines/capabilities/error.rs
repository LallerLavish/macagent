// src/engines/capabilities/error.rs
//
// CapError: structured failure mode for the capability layer.
//
// Design notes:
//   - Variants are stable wire tags (via kind_str()) so the IPC response
//     can include a machine-readable error_kind alongside the human message.
//   - MissingDependency carries an install hint string. The dispatcher
//     surfaces this verbatim so users see "install with: brew install blueutil".
//   - Panic is intentionally a variant rather than letting panics crash the
//     daemon — the dispatch_safe wrapper catches panics and converts them.
//   - Timeout is structured (carries Duration) so logs can surface how long
//     we waited, helping diagnose stuck capabilities.

use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CapError {
    /// Capability doesn't implement this action (or the action isn't
    /// meaningful for this target). E.g. calling turn_on on Volume.
    #[error("action not supported by this capability")]
    UnsupportedAction,

    /// External tool or framework not present on the system.
    /// `install_hint` is shown to the user so they know how to fix it.
    #[error("missing dependency: {tool}{}",
        install_hint.as_ref()
            .map(|h| format!(" — {}", h))
            .unwrap_or_default())]
    MissingDependency {
        tool: String,
        install_hint: Option<String>,
    },

    /// Capability call exceeded its time budget. The underlying process
    /// (if any) was killed via kill_on_drop.
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),

    /// Underlying CLI / AppleScript / FFI call returned a non-success
    /// result. The string is the captured stderr / NSError description.
    #[error("external call failed: {0}")]
    External(String),

    /// macOS denied a TCC permission (Accessibility, Automation, etc.).
    /// The user has to grant in System Settings → Privacy & Security.
    #[error("permission denied — likely needs Accessibility/Automation: {0}")]
    PermissionDenied(String),

    /// A panic was caught inside a capability. Should not happen in normal
    /// operation; surfaces to the user as "internal error" but logged with
    /// full context for the developer.
    #[error("capability panic: {0}")]
    Panic(String),

    /// Caller passed an invalid value (out-of-range volume level, etc.).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Capability is registered but probe says it can't run right now.
    /// E.g. we have a brightness backend but no display is attached.
    #[error("capability unavailable: {0}")]
    Unavailable(String),

    /// Generic catch-all. Use sparingly — prefer one of the typed variants
    /// above so the wire response gets a useful kind tag.
    #[error("internal error: {0}")]
    Internal(String),
}

impl CapError {
    /// Stable wire tag for the IPC `error_kind` field. Don't change these
    /// strings — clients may match on them.
    pub fn kind_str(&self) -> &'static str {
        match self {
            CapError::UnsupportedAction      => "UnsupportedAction",
            CapError::MissingDependency {..} => "MissingDependency",
            CapError::Timeout(_)             => "Timeout",
            CapError::External(_)            => "External",
            CapError::PermissionDenied(_)    => "PermissionDenied",
            CapError::Panic(_)               => "Panic",
            CapError::InvalidInput(_)        => "InvalidInput",
            CapError::Unavailable(_)         => "Unavailable",
            CapError::Internal(_)            => "Internal",
        }
    }

    /// True if the error is "transient" — worth retrying or reporting as a
    /// soft failure. Permission denial / missing dep are hard failures.
    pub fn is_transient(&self) -> bool {
        matches!(self, CapError::Timeout(_) | CapError::External(_))
    }
}

pub type CapResult<T> = Result<T, CapError>;

// ---- Convenience constructors -------------------------------------------

impl CapError {
    pub fn missing(tool: impl Into<String>) -> Self {
        CapError::MissingDependency {
            tool: tool.into(),
            install_hint: None,
        }
    }

    pub fn missing_with_hint(tool: impl Into<String>, hint: impl Into<String>) -> Self {
        CapError::MissingDependency {
            tool: tool.into(),
            install_hint: Some(hint.into()),
        }
    }

    pub fn external(msg: impl Into<String>) -> Self {
        CapError::External(msg.into())
    }

    pub fn permission(msg: impl Into<String>) -> Self {
        CapError::PermissionDenied(msg.into())
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        CapError::InvalidInput(msg.into())
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        CapError::Unavailable(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_strings_are_stable() {
        // If anyone changes a kind_str, this test should fail to force a
        // conscious decision (since clients may match on these strings).
        assert_eq!(CapError::UnsupportedAction.kind_str(), "UnsupportedAction");
        assert_eq!(CapError::Timeout(Duration::from_secs(1)).kind_str(), "Timeout");
        assert_eq!(CapError::missing("blueutil").kind_str(), "MissingDependency");
    }

    #[test]
    fn missing_with_hint_formats_correctly() {
        let e = CapError::missing_with_hint("blueutil", "brew install blueutil");
        let msg = e.to_string();
        assert!(msg.contains("blueutil"));
        assert!(msg.contains("brew install"));
    }

    #[test]
    fn missing_without_hint_is_clean() {
        let e = CapError::missing("blueutil");
        assert_eq!(e.to_string(), "missing dependency: blueutil");
    }

    #[test]
    fn transient_classification() {
        assert!(CapError::Timeout(Duration::from_secs(1)).is_transient());
        assert!(CapError::External("foo".into()).is_transient());
        assert!(!CapError::missing("x").is_transient());
        assert!(!CapError::permission("y").is_transient());
    }
}
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use super::error::ModeError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode {
    pub name: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub apps: AppPlan,

    #[serde(default)]
    pub system: SystemPlan,

    #[serde(default)]
    pub projects: Vec<ProjectRef>,

    #[serde(default)]
    pub activity_snapshot: Option<ActivitySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPlan {
    /// Bundle IDs to launch on switch
    pub launch: Vec<String>,

    /// If true, close apps not in `launch` on switch (hard restore)
    #[serde(default = "default_true")]
    pub close_others: bool,

    /// Per-app state — empty in M1, populated in M2
    #[serde(default)]
    pub state: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemPlan {
    pub dnd: Option<bool>,
    pub wifi: Option<bool>,
    pub bluetooth: Option<bool>,
    pub volume: Option<u8>,
    pub brightness: Option<u8>,
}

impl SystemPlan {
    /// Convert to a SystemState (same shape, different home module).
    pub fn to_state(&self) -> crate::modes::system_state::SystemState {
        crate::modes::system_state::SystemState {
            dnd: self.dnd,
            wifi: self.wifi,
            bluetooth: self.bluetooth,
            volume: self.volume,
            brightness: self.brightness,
        }
    }

    /// Build from a captured SystemState.
    pub fn from_state(state: &crate::modes::system_state::SystemState) -> Self {
        Self {
            dnd: state.dnd,
            wifi: state.wifi,
            bluetooth: state.bluetooth,
            volume: state.volume,
            brightness: state.brightness,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.dnd.is_none()
            && self.wifi.is_none()
            && self.bluetooth.is_none()
            && self.volume.is_none()
            && self.brightness.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRef {
    pub path: PathBuf,
    #[serde(default)]
    pub git: bool,
}

/// Placeholder — schema locked in M3 once dataset format is finalized
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivitySnapshot {
    pub captured_at: DateTime<Utc>,
    pub data: serde_json::Value,
}

fn default_true() -> bool { true }

/// Validate a mode name. Names are used as filenames so we keep them strict.
pub fn validate_name(name: &str) -> Result<(), ModeError> {
    if name.is_empty() {
        return Err(ModeError::InvalidName(name.into(), "empty"));
    }
    if name.len() > 64 {
        return Err(ModeError::InvalidName(name.into(), "too long (max 64)"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(ModeError::InvalidName(name.into(), "only [a-zA-Z0-9_-] allowed"));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(ModeError::InvalidName(name.into(), "cannot start with . or -"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(validate_name("work").is_ok());
        assert!(validate_name("work_mode").is_ok());
        assert!(validate_name("work-mode-2").is_ok());

        assert!(validate_name("").is_err());
        assert!(validate_name("work mode").is_err());      // space
        assert!(validate_name("work/mode").is_err());      // slash
        assert!(validate_name(".hidden").is_err());        // dotfile
        assert!(validate_name("-dash").is_err());          // leading dash
        assert!(validate_name(&"x".repeat(65)).is_err());  // too long
    }
}
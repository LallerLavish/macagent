// src/engines/capabilities/dnd.rs
//
// Do Not Disturb / Focus mode via the `shortcuts` CLI.
//
// Modern macOS (Monterey+) replaced classic DND with Focus modes, controlled
// through the Shortcuts app. The CLI tool `shortcuts` ships with macOS and
// can run any user-defined or system shortcut.
//
// Setup requirement: the user needs a shortcut named "Toggle Do Not Disturb"
// in their Shortcuts library. The Shortcuts app provides system actions to
// build this — they create a shortcut once with one action ("Set Focus →
// Do Not Disturb → Toggle"). After that this capability works.
//
// We probe by running `shortcuts list` and checking whether the expected
// shortcut name is present. If not, capability is unavailable and the
// install_hint tells the user how to add it.
//
// For state queries we don't have a clean way without parsing UI. So
// is_on() returns UnsupportedAction; the executor handles this gracefully
// by skipping query-based logic for this target.

use async_trait::async_trait;

use super::runtime::{run_command, which_exists};
use super::{BinaryCapability, CapError, CapResult};
use crate::engines::actions::intent::Target;

const SHORTCUT_NAME_TOGGLE: &str = "Toggle Do Not Disturb";
const SHORTCUT_NAME_ON: &str = "Turn On Do Not Disturb";
const SHORTCUT_NAME_OFF: &str = "Turn Off Do Not Disturb";

pub struct DndViaShortcuts;

async fn shortcut_exists(name: &str) -> bool {
    let stdout = match run_command("shortcuts", &["list"]).await {
        Ok(s) => s,
        Err(_) => return false,
    };
    stdout.lines().any(|l| l.trim().eq_ignore_ascii_case(name))
}

#[async_trait]
impl BinaryCapability for DndViaShortcuts {
    fn id(&self) -> &str {
        "core::dnd::shortcuts"
    }

    fn target(&self) -> Target {
        Target::DoNotDisturb
    }

    fn priority(&self) -> i32 {
        100
    }

    fn install_hint(&self) -> Option<&str> {
        Some("create shortcuts named 'Turn On/Off Do Not Disturb' and 'Toggle Do Not Disturb' in the Shortcuts app")
    }

    async fn is_available(&self) -> bool {
        // Two checks: shortcuts CLI exists, and at least the toggle shortcut
        // exists in the user's library. We don't require all three — if
        // they only have toggle, we'll use it for both turn_on/turn_off.
        if !which_exists("shortcuts").await {
            return false;
        }
        shortcut_exists(SHORTCUT_NAME_TOGGLE).await
            || shortcut_exists(SHORTCUT_NAME_ON).await
    }

    async fn turn_on(&self) -> CapResult<()> {
        if shortcut_exists(SHORTCUT_NAME_ON).await {
            run_command("shortcuts", &["run", SHORTCUT_NAME_ON]).await?;
            return Ok(());
        }
        // Fall back to toggle if no dedicated on shortcut exists.
        if shortcut_exists(SHORTCUT_NAME_TOGGLE).await {
            run_command("shortcuts", &["run", SHORTCUT_NAME_TOGGLE]).await?;
            return Ok(());
        }
        Err(CapError::missing_with_hint(
            SHORTCUT_NAME_ON,
            "create a shortcut named 'Turn On Do Not Disturb' in the Shortcuts app",
        ))
    }

    async fn turn_off(&self) -> CapResult<()> {
        if shortcut_exists(SHORTCUT_NAME_OFF).await {
            run_command("shortcuts", &["run", SHORTCUT_NAME_OFF]).await?;
            return Ok(());
        }
        if shortcut_exists(SHORTCUT_NAME_TOGGLE).await {
            run_command("shortcuts", &["run", SHORTCUT_NAME_TOGGLE]).await?;
            return Ok(());
        }
        Err(CapError::missing_with_hint(
            SHORTCUT_NAME_OFF,
            "create a shortcut named 'Turn Off Do Not Disturb' in the Shortcuts app",
        ))
    }

    async fn toggle(&self) -> CapResult<()> {
        if shortcut_exists(SHORTCUT_NAME_TOGGLE).await {
            run_command("shortcuts", &["run", SHORTCUT_NAME_TOGGLE]).await?;
            return Ok(());
        }
        Err(CapError::missing_with_hint(
            SHORTCUT_NAME_TOGGLE,
            "create a shortcut named 'Toggle Do Not Disturb' in the Shortcuts app",
        ))
    }

    async fn is_on(&self) -> CapResult<bool> {
        // No clean way to read this without parsing UI or private APIs.
        Err(CapError::UnsupportedAction)
    }
}
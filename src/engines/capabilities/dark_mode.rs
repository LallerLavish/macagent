// src/engines/capabilities/dark_mode.rs
//
// Dark mode toggle via AppleScript / System Events.
//
// macOS exposes appearance preferences through the scripting bridge. Unlike
// Bluetooth, this script doesn't need UI clicking — it directly sets the
// appearance preference. Stable across macOS versions since Mojave.

use async_trait::async_trait;

use super::runtime::run_applescript;
use super::{BinaryCapability, CapResult};
use crate::engines::actions::intent::Target;

pub struct DarkModeViaAppleScript;

#[async_trait]
impl BinaryCapability for DarkModeViaAppleScript {
    fn id(&self) -> &str {
        "core::dark_mode::applescript"
    }

    fn target(&self) -> Target {
        Target::DarkMode
    }

    fn priority(&self) -> i32 {
        100 // there's no better backend than scripting bridge for this
    }

    async fn is_available(&self) -> bool {
        // osascript is always available; no further probe needed.
        true
    }

    async fn turn_on(&self) -> CapResult<()> {
        let script = r#"
            tell application "System Events"
                tell appearance preferences
                    set dark mode to true
                end tell
            end tell
        "#;
        run_applescript(script).await?;
        Ok(())
    }

    async fn turn_off(&self) -> CapResult<()> {
        let script = r#"
            tell application "System Events"
                tell appearance preferences
                    set dark mode to false
                end tell
            end tell
        "#;
        run_applescript(script).await?;
        Ok(())
    }

    async fn toggle(&self) -> CapResult<()> {
        // Atomic toggle — no need for query+set.
        let script = r#"
            tell application "System Events"
                tell appearance preferences
                    set dark mode to not dark mode
                end tell
            end tell
        "#;
        run_applescript(script).await?;
        Ok(())
    }

    async fn is_on(&self) -> CapResult<bool> {
        let script = r#"
            tell application "System Events"
                tell appearance preferences
                    return dark mode
                end tell
            end tell
        "#;
        let stdout = run_applescript(script).await?;
        Ok(stdout.trim().eq_ignore_ascii_case("true"))
    }
}
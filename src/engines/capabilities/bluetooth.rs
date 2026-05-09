// src/engines/capabilities/bluetooth.rs
//
// Bluetooth power control. Three backends in priority order:
//
//   1. BluetoothViaBlueutil (priority 100)
//      Uses the third-party `blueutil` CLI. Clean, fast, atomic toggle,
//      reliable state query. Preferred when installed.
//      Install: brew install blueutil
//
//   2. BluetoothViaAppleScript (priority 50)
//      Uses System Events to click through the Control Center / menu bar.
//      Works without third-party tools but fragile — depends on the
//      current macOS UI layout. Doesn't support clean state query.
//
//   3. BluetoothViaSettings (priority 10)
//      Last resort: opens System Settings → Bluetooth pane. The user has
//      to flip it manually. Not great UX but always works as a fallback.
//
// The registry probes these at startup. Whichever has the highest priority
// AND succeeds at is_available() becomes active. The other two stay
// dormant unless an upgrade or downgrade is needed.

use async_trait::async_trait;

use super::runtime::{run_applescript, run_command, which_exists};
use super::{BinaryCapability, CapError, CapResult};
use crate::engines::actions::intent::Target;

// ---- Backend 1: blueutil (highest priority) -----------------------------

pub struct BluetoothViaBlueutil;

#[async_trait]
impl BinaryCapability for BluetoothViaBlueutil {
    fn id(&self) -> &str {
        "core::bluetooth::blueutil"
    }

    fn target(&self) -> Target {
        Target::Bluetooth
    }

    fn priority(&self) -> i32 {
        100
    }

    fn install_hint(&self) -> Option<&str> {
        Some("install with: brew install blueutil")
    }

    async fn is_available(&self) -> bool {
        which_exists("blueutil").await
    }

    async fn turn_on(&self) -> CapResult<()> {
        run_command("blueutil", &["--power", "1"]).await?;
        Ok(())
    }

    async fn turn_off(&self) -> CapResult<()> {
        run_command("blueutil", &["--power", "0"]).await?;
        Ok(())
    }

    async fn is_on(&self) -> CapResult<bool> {
        // `blueutil --power` prints "1" if on, "0" if off.
        let stdout = run_command("blueutil", &["--power"]).await?;
        let trimmed = stdout.trim();
        match trimmed {
            "1" => Ok(true),
            "0" => Ok(false),
            other => Err(CapError::external(format!(
                "unexpected blueutil output: {:?}",
                other
            ))),
        }
    }
}

// ---- Backend 2: AppleScript via System Events ---------------------------
//
// macOS exposes Bluetooth via the menu bar / Control Center. We script the
// click sequence. This is fragile across macOS versions but works without
// any third-party tools.
//
// Strategy: use the `defaults` write to ControllerPowerState, then post a
// notification to refresh the system. This is more reliable than UI scripting.

pub struct BluetoothViaAppleScript;

#[async_trait]
impl BinaryCapability for BluetoothViaAppleScript {
    fn id(&self) -> &str {
        "core::bluetooth::applescript"
    }

    fn target(&self) -> Target {
        Target::Bluetooth
    }

    fn priority(&self) -> i32 {
        50
    }

    async fn is_available(&self) -> bool {
        // osascript is always present on macOS. The deeper question is
        // whether Automation permission is granted, but we can't easily
        // test that without actually trying. Defer that to runtime.
        which_exists("osascript").await
    }

    async fn turn_on(&self) -> CapResult<()> {
        // Modern macOS (Ventura+) exposes Bluetooth via Control Center, not
        // the menu bar. The keystroke sequence varies, so we use a more
        // direct path: defaults + restart bluetoothd via blueutil-style
        // calls. Without blueutil, the cleanest pure-AppleScript route is
        // via System Events to toggle Control Center.
        //
        // This script clicks Control Center → Bluetooth → toggle. It
        // assumes English UI labels. International labels would need
        // localization which is out of scope for v1.
        let script = r#"
            tell application "System Events"
                tell process "ControlCenter"
                    try
                        click menu bar item "Control Center" of menu bar 1
                        delay 0.3
                        click button "Bluetooth" of group 1 of window "Control Center"
                        delay 0.2
                        -- Read the current state by checking the title (on/off)
                        set bt_state to (value of checkbox 1 of group 1 of window "Control Center")
                        if bt_state is 0 then
                            click checkbox 1 of group 1 of window "Control Center"
                        end if
                        click menu bar item "Control Center" of menu bar 1
                    on error err_msg
                        error "Bluetooth toggle via AppleScript failed: " & err_msg
                    end try
                end tell
            end tell
        "#;
        run_applescript(script).await?;
        Ok(())
    }

    async fn turn_off(&self) -> CapResult<()> {
        let script = r#"
            tell application "System Events"
                tell process "ControlCenter"
                    try
                        click menu bar item "Control Center" of menu bar 1
                        delay 0.3
                        click button "Bluetooth" of group 1 of window "Control Center"
                        delay 0.2
                        set bt_state to (value of checkbox 1 of group 1 of window "Control Center")
                        if bt_state is 1 then
                            click checkbox 1 of group 1 of window "Control Center"
                        end if
                        click menu bar item "Control Center" of menu bar 1
                    on error err_msg
                        error "Bluetooth toggle via AppleScript failed: " & err_msg
                    end try
                end tell
            end tell
        "#;
        run_applescript(script).await?;
        Ok(())
    }

    async fn is_on(&self) -> CapResult<bool> {
        // Reading state without blueutil is hard. We can shell out to
        // `system_profiler SPBluetoothDataType` and parse, which is
        // reliable but slow (~500ms). It's worth it for state queries
        // since they happen rarely.
        let stdout = run_command(
            "system_profiler",
            &["SPBluetoothDataType", "-detailLevel", "mini"],
        )
        .await?;
        // Look for "State: On" or "State: Off" in the output.
        for line in stdout.lines() {
            let t = line.trim();
            if let Some(state) = t.strip_prefix("State:") {
                let s = state.trim().to_lowercase();
                if s == "on" {
                    return Ok(true);
                } else if s == "off" {
                    return Ok(false);
                }
            }
        }
        Err(CapError::external(
            "could not parse Bluetooth state from system_profiler",
        ))
    }
}

// ---- Backend 3: Settings fallback (last resort) -------------------------
//
// If neither blueutil nor AppleScript automation works, we open the
// Bluetooth settings pane. The user has to flip the switch themselves.
// This isn't really automation but it's better than failing silently.

pub struct BluetoothViaSettings;

#[async_trait]
impl BinaryCapability for BluetoothViaSettings {
    fn id(&self) -> &str {
        "core::bluetooth::settings"
    }

    fn target(&self) -> Target {
        Target::Bluetooth
    }

    fn priority(&self) -> i32 {
        10 // last resort
    }

    fn install_hint(&self) -> Option<&str> {
        Some("for automated toggle, install: brew install blueutil")
    }

    async fn is_available(&self) -> bool {
        // `open` is always present.
        true
    }

    async fn turn_on(&self) -> CapResult<()> {
        run_command(
            "open",
            &["x-apple.systempreferences:com.apple.preferences.Bluetooth"],
        )
        .await?;
        // We can't actually toggle, so this is "advisory" — return an error
        // so the user knows we couldn't actually do it. The error_kind
        // surfaces via IPC so the response makes the limitation clear.
        Err(CapError::Unavailable(
            "opened Bluetooth settings; install blueutil for true automation".into(),
        ))
    }

    async fn turn_off(&self) -> CapResult<()> {
        run_command(
            "open",
            &["x-apple.systempreferences:com.apple.preferences.Bluetooth"],
        )
        .await?;
        Err(CapError::Unavailable(
            "opened Bluetooth settings; install blueutil for true automation".into(),
        ))
    }

    async fn is_on(&self) -> CapResult<bool> {
        // We can't read state from this backend.
        Err(CapError::UnsupportedAction)
    }
}
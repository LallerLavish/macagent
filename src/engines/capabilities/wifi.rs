// src/engines/capabilities/wifi.rs
//
// Wi-Fi power control via Apple's `networksetup` CLI.
//
// Why networksetup:
//   - Public Apple-shipped tool. Stable across macOS versions.
//   - No third-party dependency.
//   - Sub-50ms per call on M-series.
//
// Why not CoreWLAN FFI:
//   - CoreWLAN's setPower() is private SPI Apple has been progressively
//     locking down. networksetup wraps the same internal API and Apple
//     keeps it working.
//
// Interface auto-detection:
//   networksetup needs a hardware port name like "en0". Asking the user
//   to know that is bad UX. We probe at first use via:
//     networksetup -listallhardwareports
//   ...and find the line under "Wi-Fi" or "AirPort".
//
// We cache the detected interface for the lifetime of the daemon. If the
// user hot-swaps Wi-Fi adapters mid-session (rare), they'll need to restart.

use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::runtime::run_command;
use super::{BinaryCapability, CapError, CapResult};
use crate::engines::actions::intent::Target;

pub struct WiFiViaNetworksetup;

// Cached interface name. OnceCell ensures we probe at most once across the
// daemon's lifetime, even under concurrent requests.
static WIFI_IFACE: OnceCell<String> = OnceCell::const_new();

async fn detect_wifi_interface() -> CapResult<String> {
    // Output looks like:
    //   Hardware Port: Wi-Fi
    //   Device: en0
    //   Ethernet Address: ...
    //
    //   Hardware Port: Bluetooth PAN
    //   Device: en6
    //   ...
    let stdout = run_command("networksetup", &["-listallhardwareports"]).await?;

    let mut current_port: Option<&str> = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Hardware Port:") {
            current_port = Some(rest.trim());
        } else if let Some(rest) = trimmed.strip_prefix("Device:") {
            // We just saw "Hardware Port: X". If X is Wi-Fi or AirPort, this
            // line's device is what we want.
            if let Some(port) = current_port {
                if port.eq_ignore_ascii_case("wi-fi") || port.eq_ignore_ascii_case("airport") {
                    return Ok(rest.trim().to_string());
                }
            }
        }
    }

    Err(CapError::unavailable(
        "no Wi-Fi hardware port detected via networksetup",
    ))
}

async fn iface() -> CapResult<&'static str> {
    let s = WIFI_IFACE
        .get_or_try_init(|| async { detect_wifi_interface().await })
        .await?;
    Ok(s.as_str())
}

#[async_trait]
impl BinaryCapability for WiFiViaNetworksetup {
    fn id(&self) -> &str {
        "core::wifi::networksetup"
    }

    fn target(&self) -> Target {
        Target::WiFi
    }

    fn priority(&self) -> i32 {
        100 // highest — this is the canonical macOS interface
    }

    async fn is_available(&self) -> bool {
        // networksetup is ALWAYS present on macOS. We only check that we
        // can detect a Wi-Fi interface — on a desktop with no Wi-Fi card
        // (rare, e.g. some Mac Pros), this would return false.
        iface().await.is_ok()
    }

    async fn turn_on(&self) -> CapResult<()> {
        let i = iface().await?;
        run_command("networksetup", &["-setairportpower", i, "on"]).await?;
        Ok(())
    }

    async fn turn_off(&self) -> CapResult<()> {
        let i = iface().await?;
        run_command("networksetup", &["-setairportpower", i, "off"]).await?;
        Ok(())
    }

    async fn is_on(&self) -> CapResult<bool> {
        let i = iface().await?;
        // Output: "Wi-Fi Power (en0): On" or "Wi-Fi Power (en0): Off"
        let stdout = run_command("networksetup", &["-getairportpower", i]).await?;
        let lower = stdout.to_lowercase();
        if lower.contains(": on") {
            Ok(true)
        } else if lower.contains(": off") {
            Ok(false)
        } else {
            Err(CapError::external(format!(
                "unexpected getairportpower output: {}",
                stdout.trim()
            )))
        }
    }
}
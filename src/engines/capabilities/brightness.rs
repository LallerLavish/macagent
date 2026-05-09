// src/engines/capabilities/brightness.rs
//
// Three backends for screen brightness, in priority order:
//
//   1. ScreenBrightnessViaDisplayServices (priority 110)
//      Direct FFI to Apple's private DisplayServices.framework. Absolute
//      brightness on Apple Silicon. Sub-millisecond. What MonitorControl,
//      Lunar, and BetterDisplay use.
//
//   2. ScreenBrightnessViaCli (priority 60)
//      nriley/brightness CLI. Works on Intel + external displays.
//      Probe-aware: fails on Apple Silicon built-ins, registry skips.
//
//   3. ScreenBrightnessViaAppleScript (priority 50)
//      F1/F2 key codes. Always works. Step-only (no absolute set).

use async_trait::async_trait;

use super::runtime::{run_applescript, run_command, which_exists};
use super::{AnalogCapability, CapError, CapResult};
use crate::engines::actions::intent::Target;

// ============================================================
// CoreGraphics + DisplayServices FFI
// ============================================================

type CGDirectDisplayID = u32;
type CGError = i32;
const K_CG_ERROR_SUCCESS: CGError = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> CGError;
}

#[link(name = "DisplayServices", kind = "framework")]
extern "C" {
    fn DisplayServicesGetBrightness(
        display: CGDirectDisplayID,
        brightness: *mut f32,
    ) -> i32;
    fn DisplayServicesSetBrightness(
        display: CGDirectDisplayID,
        brightness: f32,
    ) -> i32;
    // DisplayServicesBrightnessChanged removed due to missing arm64 symbol
}

// ============================================================
// Backend 1: DisplayServices FFI (priority 110)
// ============================================================

pub struct ScreenBrightnessViaDisplayServices;

#[async_trait]
impl AnalogCapability for ScreenBrightnessViaDisplayServices {
    fn id(&self) -> &str {
        "core::screen_brightness::displayservices"
    }

    fn target(&self) -> Target {
        Target::ScreenBrightness
    }

    fn priority(&self) -> i32 {
        110
    }

    fn range(&self) -> (i32, i32) {
        (0, 100)
    }

    fn default_step(&self) -> i32 {
        10
    }

    async fn is_available(&self) -> bool {
        tokio::task::spawn_blocking(|| ds_get_brightness_pct().is_ok())
            .await
            .unwrap_or(false)
    }

    async fn set(&self, value: i32) -> CapResult<()> {
        let clamped = value.clamp(0, 100);
        tokio::task::spawn_blocking(move || ds_set_brightness_pct(clamped))
            .await
            .map_err(|e| CapError::Internal(format!("spawn_blocking: {e}")))?
    }

    async fn current(&self) -> CapResult<i32> {
        tokio::task::spawn_blocking(ds_get_brightness_pct)
            .await
            .map_err(|e| CapError::Internal(format!("spawn_blocking: {e}")))?
    }
}

fn get_active_displays() -> Result<Vec<CGDirectDisplayID>, CapError> {
    let mut count: u32 = 0;
    // SAFETY: querying count with null buffer per documented API contract.
    let err = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if err != K_CG_ERROR_SUCCESS {
        return Err(CapError::external(format!(
            "CGGetActiveDisplayList(count) failed: {}",
            err
        )));
    }
    if count == 0 {
        return Err(CapError::Unavailable("no active displays".into()));
    }

    let mut displays: Vec<CGDirectDisplayID> = vec![0; count as usize];
    // SAFETY: buffer sized to count, matching capacity passed in.
    let err = unsafe {
        CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count)
    };
    if err != K_CG_ERROR_SUCCESS {
        return Err(CapError::external(format!(
            "CGGetActiveDisplayList(fill) failed: {}",
            err
        )));
    }

    Ok(displays)
}

fn ds_get_brightness_pct() -> CapResult<i32> {
    // SAFETY: parameterless call.
    let display = unsafe { CGMainDisplayID() };
    if display == 0 {
        return Err(CapError::Unavailable("no main display".into()));
    }

    let mut brightness: f32 = 0.0;
    // SAFETY: display ID is valid (non-zero from CGMainDisplayID);
    // brightness is a stack f32 the framework writes a single f32 into.
    let result = unsafe { DisplayServicesGetBrightness(display, &mut brightness) };

    if result != 0 {
        return Err(CapError::external(format!(
            "DisplayServicesGetBrightness failed: {}",
            result
        )));
    }

    let pct = (brightness.clamp(0.0, 1.0) * 100.0).round() as i32;
    Ok(pct)
}

fn ds_set_brightness_pct(pct: i32) -> CapResult<()> {
    // SAFETY: same as above.
    let display = unsafe { CGMainDisplayID() };
    if display == 0 {
        return Err(CapError::Unavailable("no main display".into()));
    }

    let scalar = (pct.clamp(0, 100) as f32) / 100.0;

    // SAFETY: display valid; scalar is value type.
    let result = unsafe { DisplayServicesSetBrightness(display, scalar) };
    if result != 0 {
        return Err(CapError::external(format!(
            "DisplayServicesSetBrightness failed: {}",
            result
        )));
    }

    // Best effort: also update external displays. Failures here are not
    // surfaced — typical case is one external monitor that doesn't support
    // DDC-based brightness control.
    if let Ok(displays) = get_active_displays() {
        for ext in displays {
            if ext == display {
                continue;
            }
            // SAFETY: ext is a valid display ID returned by Core Graphics.
            unsafe {
                let _ = DisplayServicesSetBrightness(ext, scalar);
            }
        }
    }

    Ok(())
}

// ============================================================
// Backend 2: nriley/brightness CLI (priority 60)
// ============================================================

pub struct ScreenBrightnessViaCli;

#[async_trait]
impl AnalogCapability for ScreenBrightnessViaCli {
    fn id(&self) -> &str {
        "core::screen_brightness::cli"
    }

    fn target(&self) -> Target {
        Target::ScreenBrightness
    }

    fn priority(&self) -> i32 {
        60
    }

    fn install_hint(&self) -> Option<&str> {
        Some("install with: brew install brightness")
    }

    async fn is_available(&self) -> bool {
        if !which_exists("brightness").await {
            return false;
        }
        get_brightness_via_cli().await.is_ok()
    }

    fn range(&self) -> (i32, i32) {
        (0, 100)
    }

    fn default_step(&self) -> i32 {
        10
    }

    async fn set(&self, value: i32) -> CapResult<()> {
        let clamped = value.clamp(0, 100);
        let scalar = format!("{:.4}", clamped as f32 / 100.0);
        run_command("brightness", &[&scalar]).await?;
        Ok(())
    }

    async fn current(&self) -> CapResult<i32> {
        get_brightness_via_cli().await
    }
}

async fn get_brightness_via_cli() -> CapResult<i32> {
    let out = run_command("brightness", &["-l"]).await?;
    for line in out.lines() {
        if let Some((_, rest)) = line.split_once("brightness ") {
            if let Ok(v) = rest.trim().parse::<f32>() {
                return Ok((v.clamp(0.0, 1.0) * 100.0).round() as i32);
            }
        }
    }
    Err(CapError::external("no brightness reading found in `brightness -l`"))
}

// ============================================================
// Backend 3: AppleScript stepped (priority 50)
// ============================================================

pub struct ScreenBrightnessViaAppleScript;

#[async_trait]
impl AnalogCapability for ScreenBrightnessViaAppleScript {
    fn id(&self) -> &str {
        "core::screen_brightness::applescript"
    }

    fn target(&self) -> Target {
        Target::ScreenBrightness
    }

    fn priority(&self) -> i32 {
        50
    }

    async fn is_available(&self) -> bool {
        which_exists("osascript").await
    }

    fn range(&self) -> (i32, i32) {
        (0, 100)
    }

    fn default_step(&self) -> i32 {
        10
    }

    async fn set(&self, _value: i32) -> CapResult<()> {
        Err(CapError::invalid(
            "absolute brightness via AppleScript not supported — use 'increase' \
             or 'decrease' for stepped control",
        ))
    }

    async fn adjust(&self, delta: i32) -> CapResult<()> {
        let steps = ((delta.abs() as f32) / 6.25).round() as i32;
        let key_code = if delta > 0 { 144 } else { 145 };

        for _ in 0..steps {
            let script = format!(
                r#"tell application "System Events" to key code {}"#,
                key_code
            );
            run_applescript(&script).await?;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn current(&self) -> CapResult<i32> {
        Err(CapError::UnsupportedAction)
    }
}

// ============================================================
// Keyboard brightness — unchanged
// ============================================================

pub struct KeyboardBrightnessViaCli;

#[async_trait]
impl AnalogCapability for KeyboardBrightnessViaCli {
    fn id(&self) -> &str {
        "core::keyboard_brightness::mac-brightnessctl"
    }

    fn target(&self) -> Target {
        Target::KeyboardBrightness
    }

    fn priority(&self) -> i32 {
        100
    }

    fn install_hint(&self) -> Option<&str> {
        Some("install via: brew install mac-brightnessctl (or your equivalent)")
    }

    async fn is_available(&self) -> bool {
        which_exists("mac-brightnessctl").await
    }

    fn range(&self) -> (i32, i32) {
        (0, 100)
    }

    fn default_step(&self) -> i32 {
        10
    }

    async fn set(&self, value: i32) -> CapResult<()> {
        let clamped = value.clamp(0, 100);
        let scalar = format!("{:.2}", clamped as f32 / 100.0);
        run_command("mac-brightnessctl", &[&scalar]).await?;
        Ok(())
    }

    async fn adjust(&self, delta: i32) -> CapResult<()> {
        let arg = format!("{:+.2}", delta as f32 / 100.0);
        run_command("mac-brightnessctl", &[&arg]).await?;
        Ok(())
    }

    async fn current(&self) -> CapResult<i32> {
        Err(CapError::UnsupportedAction)
    }
}
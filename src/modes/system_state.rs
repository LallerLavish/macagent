//! System state capture and application.
//!
//! Reads and writes the 5 system-level fields work mode cares about:
//! DND, WiFi, Bluetooth, volume, brightness. Goes through the existing
//! Capabilities framework — resolves the active capability per target,
//! then calls trait methods directly.

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::engines::actions::intent::Target;
use crate::engines::capabilities::Capabilities;

/// A snapshot of the 5 system fields work mode tracks.
/// Each field is Option — None means "we couldn't read this field" or
/// "this field isn't being managed for this mode."
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemState {
    pub dnd: Option<bool>,
    pub wifi: Option<bool>,
    pub bluetooth: Option<bool>,
    pub volume: Option<u8>,
    pub brightness: Option<u8>,
}

impl SystemState {
    pub fn is_empty(&self) -> bool {
        self.dnd.is_none()
            && self.wifi.is_none()
            && self.bluetooth.is_none()
            && self.volume.is_none()
            && self.brightness.is_none()
    }

    pub fn populated_fields(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.dnd.is_some()        { out.push("dnd"); }
        if self.wifi.is_some()       { out.push("wifi"); }
        if self.bluetooth.is_some()  { out.push("bluetooth"); }
        if self.volume.is_some()     { out.push("volume"); }
        if self.brightness.is_some() { out.push("brightness"); }
        out
    }
}

/// Read all 5 system fields. Failures per field are logged and reported as
/// None — we don't fail the whole capture if one capability is unavailable.
pub async fn capture_system(caps: &Capabilities) -> SystemState {
    let dnd        = read_binary(caps, &Target::DoNotDisturb, "dnd").await;
    let wifi       = read_binary(caps, &Target::WiFi, "wifi").await;
    let bluetooth  = read_binary(caps, &Target::Bluetooth, "bluetooth").await;
    let volume     = read_analog(caps, &Target::Volume, "volume").await;
    let brightness = read_analog(caps, &Target::ScreenBrightness, "brightness").await;

    SystemState { dnd, wifi, bluetooth, volume, brightness }
}

/// Apply only the populated fields. Skips None fields.
/// Returns the list of fields that were successfully applied.
pub async fn apply_system(caps: &Capabilities, state: &SystemState) -> Vec<&'static str> {
    let mut applied = Vec::new();

    if let Some(v) = state.dnd {
        if write_binary(caps, &Target::DoNotDisturb, v, "dnd").await {
            applied.push("dnd");
        }
    }
    if let Some(v) = state.wifi {
        if write_binary(caps, &Target::WiFi, v, "wifi").await {
            applied.push("wifi");
        }
    }
    if let Some(v) = state.bluetooth {
        if write_binary(caps, &Target::Bluetooth, v, "bluetooth").await {
            applied.push("bluetooth");
        }
    }
    if let Some(v) = state.volume {
        if write_analog(caps, &Target::Volume, v, "volume").await {
            applied.push("volume");
        }
    }
    if let Some(v) = state.brightness {
        if write_analog(caps, &Target::ScreenBrightness, v, "brightness").await {
            applied.push("brightness");
        }
    }

    applied
}

// ---- Binary helpers ------------------------------------------------------

async fn read_binary(caps: &Capabilities, target: &Target, label: &str) -> Option<bool> {
    let cap = match caps.binary.read().await.resolve(target) {
        Some(c) => c,
        None => {
            debug!(field = label, "no active binary capability");
            return None;
        }
    };
    match cap.is_on().await {
        Ok(v) => Some(v),
        Err(e) => {
            debug!(field = label, error = %e, "binary read failed");
            None
        }
    }
}

async fn write_binary(caps: &Capabilities, target: &Target, value: bool, label: &str) -> bool {
    let cap = match caps.binary.read().await.resolve(target) {
        Some(c) => c,
        None => {
            warn!(field = label, "no active binary capability — cannot write");
            return false;
        }
    };
    let result = if value {
        cap.turn_on().await
    } else {
        cap.turn_off().await
    };
    match result {
        Ok(()) => true,
        Err(e) => {
            warn!(field = label, value = value, error = %e, "binary write failed");
            false
        }
    }
}

// ---- Analog helpers ------------------------------------------------------

async fn read_analog(caps: &Capabilities, target: &Target, label: &str) -> Option<u8> {
    let cap = match caps.analog.read().await.resolve(target) {
        Some(c) => c,
        None => {
            debug!(field = label, "no active analog capability");
            return None;
        }
    };
    match cap.current().await {
        Ok(v) => Some(v.clamp(0, 100) as u8),
        Err(e) => {
            debug!(field = label, error = %e, "analog read failed");
            None
        }
    }
}

async fn write_analog(caps: &Capabilities, target: &Target, value: u8, label: &str) -> bool {
    let cap = match caps.analog.read().await.resolve(target) {
        Some(c) => c,
        None => {
            warn!(field = label, "no active analog capability — cannot write");
            return false;
        }
    };
    match cap.set(value as i32).await {
        Ok(()) => true,
        Err(e) => {
            warn!(field = label, value = value, error = %e, "analog write failed");
            false
        }
    }
}
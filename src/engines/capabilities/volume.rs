// src/engines/capabilities/volume.rs
//
// System volume control via CoreAudio public API.
//
// Why CoreAudio (not AppleScript):
//   - Public, stable Apple API since OS X 10.0.
//   - Sub-millisecond per call vs. ~100ms for `osascript set volume`.
//   - No Automation permission prompt.
//   - Reliable state query (current level + mute state).
//
// Architecture:
//   - All FFI is confined to this file. No raw pointers leak out.
//   - Every `unsafe` block has a SAFETY comment explaining the invariants.
//   - FFI calls run inside `tokio::task::spawn_blocking` because CoreAudio
//     can briefly block on the audio HAL — we don't want to stall the
//     tokio runtime thread.
//
// Required dependency in Cargo.toml:
//   coreaudio-sys = "0.2"

use std::mem;
use std::ptr;

use async_trait::async_trait;
use coreaudio_sys::{
    kAudioDevicePropertyMute,
    kAudioDevicePropertyVolumeScalar,
    kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMaster,
    kAudioObjectPropertyScopeOutput,
    kAudioObjectSystemObject,
    AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize,
    AudioObjectID,
    AudioObjectPropertyAddress,
    AudioObjectSetPropertyData,
    OSStatus,
};

use super::{AnalogCapability, CapError, CapResult};
use crate::engines::actions::intent::Target;

pub struct VolumeViaCoreAudio;

#[async_trait]
impl AnalogCapability for VolumeViaCoreAudio {
    fn id(&self) -> &str {
        "core::volume::coreaudio"
    }

    fn target(&self) -> Target {
        Target::Volume
    }

    fn priority(&self) -> i32 {
        100
    }

    fn range(&self) -> (i32, i32) {
        (0, 100)
    }

    fn default_step(&self) -> i32 {
        10
    }

    async fn is_available(&self) -> bool {
        // Probe by trying to read the current volume. If anything in the
        // HAL is misconfigured (no audio device, etc.), this returns false
        // and the registry skips this backend.
        tokio::task::spawn_blocking(|| volume_get_percent().is_ok())
            .await
            .unwrap_or(false)
    }

    async fn set(&self, value: i32) -> CapResult<()> {
        let clamped = value.clamp(0, 100);
        tokio::task::spawn_blocking(move || volume_set_percent(clamped))
            .await
            .map_err(|e| CapError::Internal(format!("spawn_blocking: {e}")))?
    }

    async fn current(&self) -> CapResult<i32> {
        tokio::task::spawn_blocking(volume_get_percent)
            .await
            .map_err(|e| CapError::Internal(format!("spawn_blocking: {e}")))?
    }
}

// ---- FFI internals (sync, called from spawn_blocking) -------------------

/// Resolve the AudioObjectID of the system's default output device
/// (speakers, headphones, AirPods, whatever's currently selected).
fn default_output_device() -> CapResult<AudioObjectID> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let mut device_id: AudioObjectID = 0;
    let mut size: u32 = mem::size_of::<AudioObjectID>() as u32;

    // SAFETY:
    // - kAudioObjectSystemObject is a stable Apple-defined constant.
    // - &address is a valid pointer to a properly-initialized struct.
    // - device_id is a stack u32 we write into.
    // - size is initialized to the buffer size and the call updates it
    //   to bytes-written (we ignore the updated value).
    // - The 4th arg (qualifier data) is null with size 0, which is what
    //   this property expects.
    let status: OSStatus = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &address,
            0,
            ptr::null(),
            &mut size,
            &mut device_id as *mut _ as *mut _,
        )
    };

    if status != 0 {
        return Err(CapError::external(format!(
            "AudioObjectGetPropertyData(default output) failed: OSStatus {}",
            status
        )));
    }
    if device_id == 0 {
        return Err(CapError::Unavailable("no default output device".into()));
    }
    Ok(device_id)
}

/// Read current volume as 0..=100.
fn volume_get_percent() -> CapResult<i32> {
    let device = default_output_device()?;

    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyVolumeScalar,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let mut size: u32 = mem::size_of::<f32>() as u32;
    let mut volume_scalar: f32 = 0.0;

    // SAFETY: address is a valid struct pointer; volume_scalar is a
    // stack-allocated f32 that CoreAudio writes a single 32-bit float
    // into (size matches sizeof(f32)).
    let status: OSStatus = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            &mut size,
            &mut volume_scalar as *mut _ as *mut _,
        )
    };

    if status != 0 {
        return Err(CapError::external(format!(
            "AudioObjectGetPropertyData(volume) failed: OSStatus {}",
            status
        )));
    }

    // CoreAudio uses 0.0..=1.0 scalar; convert to integer percent.
    let pct = (volume_scalar.clamp(0.0, 1.0) * 100.0).round() as i32;
    Ok(pct)
}

/// Set volume (0..=100, assumed clamped).
fn volume_set_percent(pct: i32) -> CapResult<()> {
    let device = default_output_device()?;
    let scalar: f32 = (pct as f32 / 100.0).clamp(0.0, 1.0);

    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyVolumeScalar,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let size: u32 = mem::size_of::<f32>() as u32;

    // SAFETY: address valid struct; scalar is a stack f32; size matches.
    // CoreAudio reads exactly `size` bytes from the input pointer.
    let status: OSStatus = unsafe {
        AudioObjectSetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            size,
            &scalar as *const _ as *const _,
        )
    };

    if status != 0 {
        return Err(CapError::external(format!(
            "AudioObjectSetPropertyData(volume) failed: OSStatus {}",
            status
        )));
    }

    // If user just set volume to non-zero, ensure it's not muted —
    // otherwise we silently fail to be audible. Best-effort; ignore errors
    // since not every device supports the mute property.
    if pct > 0 {
        let _ = set_mute(device, false);
    }
    Ok(())
}

/// Set mute state. Best-effort: not all devices expose mute.
fn set_mute(device: AudioObjectID, mute: bool) -> CapResult<()> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyMute,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMaster,
    };

    // Probe first: not every device exposes the mute property.
    let mut size: u32 = 0;
    // SAFETY: read-only size query; address is valid.
    let probe_status = unsafe {
        AudioObjectGetPropertyDataSize(device, &address, 0, ptr::null(), &mut size)
    };
    if probe_status != 0 {
        // Property not supported — silently treat as success.
        return Ok(());
    }

    let mute_val: u32 = if mute { 1 } else { 0 };
    let size: u32 = mem::size_of::<u32>() as u32;

    // SAFETY: standard SetPropertyData with a u32 buffer.
    let status: OSStatus = unsafe {
        AudioObjectSetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            size,
            &mute_val as *const _ as *const _,
        )
    };

    if status != 0 {
        return Err(CapError::external(format!(
            "AudioObjectSetPropertyData(mute) failed: OSStatus {}",
            status
        )));
    }
    Ok(())
}

// ---- Tests ---------------------------------------------------------------
//
// Tests touch the live audio HAL — they read current volume and verify the
// shape of the response. They don't change volume because that would be
// disruptive during a `cargo test` run.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_volume_in_range() {
        let cap = VolumeViaCoreAudio;
        if !cap.is_available().await {
            // No audio device or HAL issue — skip rather than fail.
            eprintln!("audio HAL unavailable; skipping test");
            return;
        }
        let v = cap.current().await.unwrap();
        assert!((0..=100).contains(&v), "volume {} not in 0..=100", v);
    }

    #[test]
    fn id_is_stable() {
        assert_eq!(VolumeViaCoreAudio.id(), "core::volume::coreaudio");
    }
}
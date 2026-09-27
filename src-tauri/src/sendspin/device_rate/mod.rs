//! Match the output device's hardware sample rate to the incoming stream.
//!
//! cpal opens an output stream at whatever rate the server negotiated, but it
//! never changes the device's own nominal rate. On macOS a mismatch is then
//! resolved silently by the `CoreAudio` HAL resampling into the rate set in
//! Audio MIDI Setup — so a 96 kHz stream played on a DAC left at 44.1 kHz is
//! quietly converted back down before it reaches the hardware. Switching the
//! device to the stream's rate first keeps the path bit-perfect.
//!
//! Best-effort by design: any failure is logged and playback proceeds exactly
//! as before, falling back to HAL resampling.

// CoreAudio property access is FFI; lift the crate-wide `unsafe_code` deny
// for this module only, as volume_control does.
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
mod macos;

/// Switch `device`'s nominal sample rate to `sample_rate` when it supports it.
///
/// A no-op when the device already runs at that rate, when it does not
/// support it, and on platforms other than macOS.
pub fn match_device_rate(device: &cpal::Device, sample_rate: u32) {
    #[cfg(target_os = "macos")]
    macos::match_device_rate(device, sample_rate);
    #[cfg(not(target_os = "macos"))]
    let _ = (device, sample_rate);
}

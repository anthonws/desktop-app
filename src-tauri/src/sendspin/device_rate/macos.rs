//! macOS implementation: set `kAudioDevicePropertyNominalSampleRate`.

use coreaudio_sys::*;
use cpal::traits::DeviceTrait;
use objc2_core_foundation::CFString;
use std::mem;
use std::ptr;
use std::time::{Duration, Instant};

/// Nominal rates are integral in practice, but they travel as `Float64`.
const RATE_TOLERANCE_HZ: f64 = 0.5;

/// How long to wait for the hardware to report the new rate. The switch is
/// asynchronous, and opening the stream before it lands would let the HAL
/// configure against the old rate.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
const SETTLE_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    AlreadyMatched,
    Unsupported,
    Switch,
}

pub fn match_device_rate(device: &cpal::Device, sample_rate: u32) {
    let target = f64::from(sample_rate);

    let Some(uid) = device.id().ok().map(|id| id.id().to_string()) else {
        log::debug!("[DeviceRate] Output device has no id; leaving its rate alone");
        return;
    };
    let Some(device_id) = device_id_for_uid(&uid) else {
        log::warn!("[DeviceRate] Could not resolve CoreAudio device for {uid}");
        return;
    };
    let Some(current) = nominal_rate(device_id) else {
        log::warn!("[DeviceRate] Could not read the nominal rate of {uid}");
        return;
    };
    let available = available_rates(device_id).unwrap_or_default();
    log::debug!("[DeviceRate] {uid} at {current} Hz; advertises {available:?}");

    match plan_switch(current, target, &available) {
        Plan::AlreadyMatched => {
            log::debug!("[DeviceRate] Device already at {current} Hz; no switch needed");
        }
        Plan::Unsupported => {
            log::info!(
                "[DeviceRate] Device does not support {target} Hz (running at {current} Hz); \
                 CoreAudio will resample"
            );
        }
        Plan::Switch => {
            if !set_nominal_rate(device_id, target) {
                log::warn!("[DeviceRate] Failed to switch device from {current} to {target} Hz");
                return;
            }
            if wait_for_rate(device_id, target) {
                log::info!("[DeviceRate] Switched output device from {current} to {target} Hz");
            } else {
                log::warn!(
                    "[DeviceRate] Requested {target} Hz but the device had not settled after \
                     {} ms; continuing",
                    SETTLE_TIMEOUT.as_millis()
                );
            }
        }
    }
}

/// Decide what to do, given the device's current rate and its advertised ranges.
///
/// Discrete rates arrive as ranges whose minimum equals their maximum; some
/// devices report a continuous span instead.
fn plan_switch(current: f64, target: f64, available: &[(f64, f64)]) -> Plan {
    if (current - target).abs() < RATE_TOLERANCE_HZ {
        return Plan::AlreadyMatched;
    }
    let supported = available
        .iter()
        .any(|&(min, max)| target >= min - RATE_TOLERANCE_HZ && target <= max + RATE_TOLERANCE_HZ);
    if supported {
        Plan::Switch
    } else {
        Plan::Unsupported
    }
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn device_id_for_uid(uid: &str) -> Option<AudioDeviceID> {
    let cf_uid = CFString::from_str(uid);
    let uid_ref: *const CFString = ptr::from_ref(&*cf_uid);
    let addr = address(kAudioHardwarePropertyTranslateUIDToDevice);
    let mut device_id: AudioDeviceID = kAudioObjectUnknown;
    let mut size = mem::size_of::<AudioDeviceID>() as u32;

    // SAFETY: the qualifier is a pointer to a live CFStringRef (cf_uid outlives
    // the call) and the output buffer is exactly one AudioDeviceID.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &raw const addr,
            mem::size_of::<*const CFString>() as u32,
            (&raw const uid_ref).cast(),
            &raw mut size,
            ptr::addr_of_mut!(device_id).cast(),
        )
    };
    (status == 0 && device_id != kAudioObjectUnknown).then_some(device_id)
}

fn nominal_rate(device_id: AudioDeviceID) -> Option<f64> {
    let addr = address(kAudioDevicePropertyNominalSampleRate);
    let mut rate: f64 = 0.0;
    let mut size = mem::size_of::<f64>() as u32;

    // SAFETY: the output buffer is exactly one Float64.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &raw const addr,
            0,
            ptr::null(),
            &raw mut size,
            ptr::addr_of_mut!(rate).cast(),
        )
    };
    (status == 0).then_some(rate)
}

fn available_rates(device_id: AudioDeviceID) -> Option<Vec<(f64, f64)>> {
    let addr = address(kAudioDevicePropertyAvailableNominalSampleRates);
    let mut size: u32 = 0;

    // SAFETY: querying the size only; no output buffer is written.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(device_id, &raw const addr, 0, ptr::null(), &raw mut size)
    };
    if status != 0 || size == 0 {
        return None;
    }

    let count = size as usize / mem::size_of::<AudioValueRange>();
    let mut ranges = vec![
        AudioValueRange {
            mMinimum: 0.0,
            mMaximum: 0.0,
        };
        count
    ];

    // SAFETY: the buffer holds `count` AudioValueRange entries, matching `size`.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &raw const addr,
            0,
            ptr::null(),
            &raw mut size,
            ranges.as_mut_ptr().cast(),
        )
    };
    (status == 0).then(|| ranges.iter().map(|r| (r.mMinimum, r.mMaximum)).collect())
}

fn set_nominal_rate(device_id: AudioDeviceID, rate: f64) -> bool {
    let addr = address(kAudioDevicePropertyNominalSampleRate);

    // SAFETY: the input buffer is exactly one Float64.
    let status = unsafe {
        AudioObjectSetPropertyData(
            device_id,
            &raw const addr,
            0,
            ptr::null(),
            mem::size_of::<f64>() as u32,
            ptr::addr_of!(rate).cast(),
        )
    };
    status == 0
}

fn wait_for_rate(device_id: AudioDeviceID, target: f64) -> bool {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while Instant::now() < deadline {
        if nominal_rate(device_id).is_some_and(|r| (r - target).abs() < RATE_TOLERANCE_HZ) {
            return true;
        }
        std::thread::sleep(SETTLE_POLL);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // A synthetic DAC that reports discrete rates only.
    const DISCRETE: &[(f64, f64)] = &[
        (44_100.0, 44_100.0),
        (48_000.0, 48_000.0),
        (96_000.0, 96_000.0),
        (192_000.0, 192_000.0),
        (384_000.0, 384_000.0),
        (768_000.0, 768_000.0),
    ];

    #[test]
    fn no_switch_when_already_at_the_stream_rate() {
        assert_eq!(
            plan_switch(44_100.0, 44_100.0, DISCRETE),
            Plan::AlreadyMatched
        );
    }

    #[test]
    fn tolerates_float_noise_in_the_reported_rate() {
        assert_eq!(
            plan_switch(44_100.000_001, 44_100.0, DISCRETE),
            Plan::AlreadyMatched
        );
    }

    #[test]
    fn switches_down_from_a_pinned_high_rate() {
        // The exact failure seen in the field: DAC pinned at 768 kHz in Audio
        // MIDI Setup, 44.1 kHz stream — the HAL was upsampling ~17x.
        assert_eq!(plan_switch(768_000.0, 44_100.0, DISCRETE), Plan::Switch);
    }

    #[test]
    fn switches_up_to_a_supported_hi_res_rate() {
        assert_eq!(plan_switch(44_100.0, 96_000.0, DISCRETE), Plan::Switch);
    }

    #[test]
    fn leaves_the_device_alone_for_an_unsupported_rate() {
        // 88.2 kHz is not in this device's list; switching would fail, so let
        // the HAL resample as it did before.
        assert_eq!(plan_switch(44_100.0, 88_200.0, DISCRETE), Plan::Unsupported);
    }

    #[test]
    fn accepts_a_rate_inside_a_continuous_range() {
        let continuous = &[(8_000.0, 192_000.0)];
        assert_eq!(plan_switch(48_000.0, 96_000.0, continuous), Plan::Switch);
    }

    /// Read-only probe of the real default output device. Hardware-dependent,
    /// so opt-in: `cargo test reads_the_default_output_device -- --ignored --nocapture`.
    /// Exercises the UID translation and both property reads; never writes.
    #[test]
    #[ignore = "needs a real CoreAudio output device"]
    fn reads_the_default_output_device() {
        use cpal::traits::HostTrait;

        let device = cpal::default_host()
            .default_output_device()
            .expect("no default output device");
        let uid = device.id().expect("device has no id").id().to_string();
        let device_id = device_id_for_uid(&uid).expect("UID did not resolve to a device");
        let current = nominal_rate(device_id).expect("could not read nominal rate");
        let available = available_rates(device_id).expect("could not read available rates");

        println!("uid       = {uid}");
        println!("device_id = {device_id}");
        println!("current   = {current} Hz");
        println!("available = {available:?}");

        assert!(current > 0.0);
        assert!(!available.is_empty());
        assert_eq!(
            plan_switch(current, current, &available),
            Plan::AlreadyMatched
        );
    }

    #[test]
    fn treats_an_empty_range_list_as_unsupported() {
        // If the ranges could not be read, never guess — leave the device alone.
        assert_eq!(plan_switch(48_000.0, 96_000.0, &[]), Plan::Unsupported);
    }
}

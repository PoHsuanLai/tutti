//! The audio-callback side of metering: measure once, tap.

use super::{AudioTap, MeterWriter};

/// Measures `meter` and feeds `tap` from one interleaved stereo output buffer.
///
/// Stereo only: this is the master-bus tap, not a general meter. Every frame
/// of `output` is metered and tapped, whatever its length.
///
/// Called from the audio callback after DSP. Both consumers are opt-in: with
/// the meter disabled and the tap closed this costs two atomic loads.
///
/// RT-safe: no allocation, no locks (the tap `try_lock`s and skips).
#[inline]
pub fn meter_output(output: &[f32], meter: &mut MeterWriter, tap: &AudioTap) {
    if meter.is_enabled() {
        meter.measure(output);
    }
    tap.push(output);
}

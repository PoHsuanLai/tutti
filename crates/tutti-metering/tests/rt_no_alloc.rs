//! Regression gate for RT-safety: metering's audio-thread path must not
//! allocate. Covers `meter_output`, which the CPAL callback runs every buffer
//! — both the peak/RMS measure and the analysis-tap push.

use assert_no_alloc::AllocDisabler;
use tutti_metering::{AudioTap, MasterMeter, MeterReading, meter_output};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn interleaved_stereo(frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let s = (i as f32 / frames as f32).sin();
        out.push(s);
        out.push(s * 0.5);
    }
    out
}

#[test]
fn meter_output_is_allocation_free_with_meter_enabled() {
    let (mut writer, meter) = MasterMeter::pair();
    meter.enable();
    let tap = AudioTap::new();

    let buffer = interleaved_stereo(512);

    meter_output(&buffer, &mut writer, &tap);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            meter_output(&buffer, &mut writer, &tap);
        }
    });

    // The meter actually ran — a sine at ±1.0 has a nonzero peak.
    assert!(meter.get().peak_left.get() > 0.0, "meter published nothing");
}

#[test]
fn meter_output_is_allocation_free_with_tap_open() {
    let (mut writer, meter) = MasterMeter::pair();
    meter.enable();
    let tap = AudioTap::new();
    // The ring holds ~3 s; draining is the consumer's job, so this also
    // exercises the ring-full drop path once it saturates.
    // `expect`, not a discard: the whole point of this test is that the tap is
    // OPEN while `meter_output` runs, so a silently-unopened tap would make it
    // pass by measuring the closed path.
    let _consumer = tap.open().expect("a fresh tap opens");

    let buffer = interleaved_stereo(512);

    meter_output(&buffer, &mut writer, &tap);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            meter_output(&buffer, &mut writer, &tap);
        }
    });
}

#[test]
fn meter_output_is_allocation_free_with_nothing_enabled() {
    // Meter disabled, tap closed — the pass over the buffer must be skipped entirely.
    let (mut writer, meter) = MasterMeter::pair();
    let tap = AudioTap::new();

    let buffer = interleaved_stereo(256);

    meter_output(&buffer, &mut writer, &tap);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            meter_output(&buffer, &mut writer, &tap);
        }
    });

    assert_eq!(
        meter.get(),
        MeterReading::default(),
        "disabled meter published readings"
    );
}

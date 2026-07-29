//! Regression gate for RT-safety: metering's audio-thread path must not
//! allocate. Covers `meter_output`, which the CPAL callback runs every buffer
//! — both the peak/RMS measure and the analysis-tap push.

use assert_no_alloc::AllocDisabler;
use tutti_core::metering::{meter_output, AudioTap, MasterMeter, MeteringContext};

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
    let meter = MasterMeter::new();
    meter.enable();
    let tap = AudioTap::new();

    let buffer = interleaved_stereo(512);
    let mut ctx = MeteringContext::new();

    meter_output(&buffer, 512, &meter, &tap, &mut ctx);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            meter_output(&buffer, 512, &meter, &tap, &mut ctx);
        }
    });

    // The meter actually ran — a sine at ±1.0 has a nonzero peak.
    let (peak_l, _, _, _) = meter.get();
    assert!(peak_l > 0.0, "meter published nothing");
}

#[test]
fn meter_output_is_allocation_free_with_tap_open() {
    let meter = MasterMeter::new();
    meter.enable();
    let tap = AudioTap::new();
    // The ring holds ~3 s; draining is the consumer's job, so this also
    // exercises the ring-full drop path once it saturates.
    // `expect`, not a discard: the whole point of this test is that the tap is
    // OPEN while `meter_output` runs, so a silently-unopened tap would make it
    // pass by measuring the closed path.
    let _consumer = tap.open().expect("a fresh tap opens");

    let buffer = interleaved_stereo(512);
    let mut ctx = MeteringContext::new();

    meter_output(&buffer, 512, &meter, &tap, &mut ctx);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            meter_output(&buffer, 512, &meter, &tap, &mut ctx);
        }
    });
}

#[test]
fn meter_output_is_allocation_free_with_nothing_enabled() {
    // Meter disabled, tap closed — the deinterleave must be skipped entirely.
    let meter = MasterMeter::new();
    let tap = AudioTap::new();

    let buffer = interleaved_stereo(256);
    let mut ctx = MeteringContext::new();

    meter_output(&buffer, 256, &meter, &tap, &mut ctx);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            meter_output(&buffer, 256, &meter, &tap, &mut ctx);
        }
    });

    let (peak_l, peak_r, rms_l, rms_r) = meter.get();
    assert_eq!(
        (peak_l, peak_r, rms_l, rms_r),
        (0.0, 0.0, 0.0, 0.0),
        "disabled meter published readings"
    );
}

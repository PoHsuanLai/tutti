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

/// `meter_output` hands the tap the whole block: every frame arrives, in
/// order, and the last one is the block's last.
///
/// Mutation (run): `tap.push(&output[..output.len() / 2])` in `rt.rs` → the
/// count is 500, not 1000 → fails.
#[test]
fn meter_output_taps_the_whole_block() {
    const FRAMES: usize = 1000;
    let (mut writer, _meter) = MasterMeter::pair();
    let tap = AudioTap::new();
    let mut cons = tap.open().expect("a fresh tap opens");
    let block: Vec<f32> = (0..FRAMES).flat_map(|i| [i as f32, -(i as f32)]).collect();

    meter_output(&block, &mut writer, &tap);

    assert_eq!(cons.occupied_len(), FRAMES);
    let mut last = None;
    let mut n = 0;
    while let Some(f) = cons.try_pop() {
        last = Some(f);
        n += 1;
    }
    assert_eq!(n, FRAMES);
    assert_eq!(last, Some(((FRAMES - 1) as f32, -((FRAMES - 1) as f32))));
}

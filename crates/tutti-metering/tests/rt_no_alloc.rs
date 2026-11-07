//! Regression gate for RT-safety: the meter writer's audio-thread path must
//! not allocate. `tutti-engine`'s `Engine` runs `MeterWriter::measure` on
//! every block it renders (its own gate is `rt_no_alloc_engine`).

use assert_no_alloc::AllocDisabler;
use tutti_metering::MasterMeter;

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
fn measure_is_allocation_free_with_meter_enabled() {
    let (mut writer, meter) = MasterMeter::pair();
    meter.enable();

    let buffer = interleaved_stereo(512);

    writer.measure(&buffer);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            if writer.is_enabled() {
                writer.measure(&buffer);
            }
        }
    });

    // The meter actually ran — a sine at ±1.0 has a nonzero peak.
    assert!(meter.get().peak_left.get() > 0.0, "meter published nothing");
}

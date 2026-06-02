//! Regression gate: `AutomationLane::process` must not allocate.
//!
//! Each lane evaluates its envelope per-sample inside the audio callback —
//! a regression that touches the alloc/dealloc path (e.g. caching the
//! envelope lookup into a `Vec`, or rebuilding the loop range on each
//! sample) would show up here as a panic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use audio_automation::{AutomationEnvelope, AutomationPoint};
use tutti_automation::AutomationLane;
use tutti_core::dsp::F32x;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SampleRate, TransportReader};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[derive(Clone)]
struct MockTransport {
    beat: Arc<AtomicU64>,
    loop_range: Option<(f64, f64)>,
}

impl MockTransport {
    fn new(beat: f64) -> Self {
        Self {
            beat: Arc::new(AtomicU64::new(beat.to_bits())),
            loop_range: None,
        }
    }

    fn with_loop(beat: f64, start: f64, end: f64) -> Self {
        Self {
            beat: Arc::new(AtomicU64::new(beat.to_bits())),
            loop_range: Some((start, end)),
        }
    }
}

impl TransportReader for MockTransport {
    fn current_beat(&self) -> f64 {
        f64::from_bits(self.beat.load(Ordering::Relaxed))
    }
    fn is_loop_enabled(&self) -> bool {
        self.loop_range.is_some()
    }
    fn get_loop_range(&self) -> Option<(f64, f64)> {
        self.loop_range
    }
    fn is_playing(&self) -> bool {
        true
    }
    fn is_recording(&self) -> bool {
        false
    }
    fn is_in_preroll(&self) -> bool {
        false
    }
    fn tempo(&self) -> tutti_core::Bpm {
        tutti_core::Bpm(120.0)
    }
}

fn ramp_envelope() -> AutomationEnvelope<&'static str> {
    let mut env: AutomationEnvelope<&str> = AutomationEnvelope::new("volume");
    env.add_point(AutomationPoint::new(0.0, 0.0));
    env.add_point(AutomationPoint::new(4.0, 1.0));
    env.add_point(AutomationPoint::new(8.0, 0.5));
    env
}

#[test]
fn automation_lane_process_is_allocation_free() {
    let mut lane = AutomationLane::new(ramp_envelope(), MockTransport::new(2.0));
    lane.set_sample_rate(SampleRate(48_000.0));

    let mut output_simd = vec![F32x::ZERO; 16];

    // Warm up — first process may prime envelope state.
    {
        let input = BufferRef::empty();
        let mut output = BufferMut::new(&mut output_simd);
        lane.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let input = BufferRef::empty();
            let mut output = BufferMut::new(&mut output_simd);
            lane.process(64, &input, &mut output);
        }
    });
}

#[test]
fn automation_lane_process_with_loop_is_allocation_free() {
    // Loop-wrapped path exercises `get_value_looped` per sample.
    let mut lane = AutomationLane::new(ramp_envelope(), MockTransport::with_loop(10.0, 4.0, 8.0));
    lane.set_sample_rate(SampleRate(48_000.0));

    let mut output_simd = vec![F32x::ZERO; 16];

    {
        let input = BufferRef::empty();
        let mut output = BufferMut::new(&mut output_simd);
        lane.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let input = BufferRef::empty();
            let mut output = BufferMut::new(&mut output_simd);
            lane.process(64, &input, &mut output);
        }
    });
}

#[test]
fn automation_lane_tick_is_allocation_free() {
    let mut lane = AutomationLane::new(ramp_envelope(), MockTransport::new(2.0));
    lane.set_sample_rate(SampleRate(48_000.0));

    let mut output = [0.0f32; 1];
    lane.tick(&[], &mut output);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            lane.tick(&[], &mut output);
        }
    });
}

//! Regression gate: sampler hot paths must not allocate per-buffer.
//!
//! Covers in-memory `SamplerUnit::process` + `tick` (the workhorse
//! playback unit).
//!
//! `StreamingSamplerUnit` is not covered here — it requires a real
//! `RegionReader` from the butler. Its non-alloc safety is guarded by
//! the streaming-buffer regression tests in `tutti-sampler/butler`
//! instead.
//!
//! `TimeStretchUnit` (`stretch::Unit`) is covered here too: its
//! fixed-capacity `RtScratch` scratch buffers make `process` non-allocating
//! for any block size up to the preallocated maximum.

use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferMut, BufferRef, BufferVec, SampleRate, SignalFrame, Wave};
use tutti_sampler::stretch::{Algorithm, Unit as TimeStretchUnit};
use tutti_sampler::SamplerUnit;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build a stereo `Wave` of `duration_secs` filled with a 440 Hz sine.
fn sine_wave(duration_secs: f64, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(2, sample_rate, duration_secs);
    let len = wave.len();
    for i in 0..len {
        let t = i as f64 / sample_rate;
        let s = (t * 440.0 * core::f64::consts::TAU).sin() as f32 * 0.5;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    Arc::new(wave)
}

#[test]
fn sampler_unit_process_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);
    let mut node = SamplerUnit::new(wave);
    node.set_sample_rate(SampleRate(48_000.0));

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm-up — drains the position update branch.
    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

#[test]
fn sampler_unit_tick_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);
    let mut node = SamplerUnit::new(wave);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut output = [0.0f32; 2];
    for _ in 0..256 {
        node.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            node.tick(&[], &mut output);
        }
    });
}

/// Minimal constant source so the time-stretch unit has something to pull.
#[derive(Clone)]
struct ConstSource;

impl AudioUnit for ConstSource {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        2
    }
    fn reset(&mut self) {}
    fn set_sample_rate(&mut self, _: SampleRate) {}
    fn tick(&mut self, _: &[f32], output: &mut [f32]) {
        if output.len() >= 2 {
            output[0] = 0.25;
            output[1] = 0.25;
        }
    }
    fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, 0.25);
            output.set_f32(1, i, 0.25);
        }
    }
    fn get_id(&self) -> u64 {
        424242
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
        SignalFrame::new(2)
    }
    fn footprint(&self) -> usize {
        0
    }
}

#[test]
fn time_stretch_process_is_allocation_free() {
    let mut node = TimeStretchUnit::new(Box::new(ConstSource), 48_000.0);
    node.set_sample_rate(SampleRate(48_000.0));
    node.set_stretch_factor(1.5);
    assert_eq!(node.algorithm(), Algorithm::PhaseVocoder);

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm up past the phase-vocoder fill-up latency so `process` is on its
    // steady-state path inside the guarded loop.
    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

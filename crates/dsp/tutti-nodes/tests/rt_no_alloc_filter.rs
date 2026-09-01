//! Regression gate: filter / EQ nodes must not allocate per-buffer.
//!
//! Covers `LadderFilterNode`, `EqBandNode`, `SvfFilterNode`, and
//! `StereoSvfFilterNode`. The shared risk is the coefficient cache: the
//! `maybe_update` hot path conditionally recomputes coeffs when params
//! change. A regression that recomputes every sample is still alloc-free
//! today, but a regression that *materialises* a coeff history (e.g. for
//! parameter smoothing) would land here.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, SampleRate};
use tutti_nodes::{
    EqBandNode, LadderFilterNode, LadderType, StereoLadderFilterNode, StereoSvfFilterNode,
    SvfFilterNode, SvfType,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn fill_with_signal(vec: &mut BufferVec, amplitude: f32) {
    let mut buf = vec.buffer_mut();
    let channels = buf.channels();
    for c in 0..channels {
        for i in 0..64 {
            let sample = if i % 2 == 0 { amplitude } else { -amplitude };
            buf.set_f32(c, i, sample);
        }
    }
}

#[test]
fn ladder_filter_process_is_allocation_free() {
    let mut node = LadderFilterNode::<f64>::new(LadderType::LP24, 1_000.0, 0.6);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(1);
    let mut output_vec = BufferVec::new(1);
    fill_with_signal(&mut input_vec, 0.5);

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
fn eq_band_process_is_allocation_free() {
    let mut node = EqBandNode::<f64>::new(SvfType::Bell, 1_000.0, 1.0, 6.0);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(1);
    let mut output_vec = BufferVec::new(1);
    fill_with_signal(&mut input_vec, 0.5);

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
fn svf_filter_mono_process_is_allocation_free() {
    let mut node = SvfFilterNode::<f64>::new(SvfType::LowPass, 800.0, 0.707);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(1);
    let mut output_vec = BufferVec::new(1);
    fill_with_signal(&mut input_vec, 0.5);

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
fn svf_filter_stereo_process_is_allocation_free() {
    let mut node = StereoSvfFilterNode::<f64>::new(SvfType::HighPass, 200.0, 0.707);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 0.5);

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
fn svf_filter_wide_6ch_process_is_allocation_free() {
    // The per-channel integrator Vec must be built at construction — a
    // regression that (re)allocated it per buffer, or per-channel scratch in
    // `process`, would trip here.
    let mut node = StereoSvfFilterNode::<f64>::with_channels(6, SvfType::LowPass, 800.0, 0.707);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(6);
    let mut output_vec = BufferVec::new(6);
    fill_with_signal(&mut input_vec, 0.5);

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
fn ladder_filter_wide_6ch_process_is_allocation_free() {
    let mut node = StereoLadderFilterNode::<f64>::with_channels(6, LadderType::LP24, 1_000.0, 0.6);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(6);
    let mut output_vec = BufferVec::new(6);
    fill_with_signal(&mut input_vec, 0.5);

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
fn svf_filter_with_parameter_changes_is_allocation_free() {
    // Exercises the `maybe_update` branch: every iteration nudges the
    // frequency atomic, forcing a coefficient recomputation.
    let mut node = SvfFilterNode::<f64>::new(SvfType::Bell, 1_000.0, 1.0);
    node.set_sample_rate(SampleRate(48_000.0));
    let freq = node.frequency();

    let mut input_vec = BufferVec::new(1);
    let mut output_vec = BufferVec::new(1);
    fill_with_signal(&mut input_vec, 0.5);

    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        let mut hz = 500.0f32;
        for _ in 0..2_000 {
            hz = if hz > 4_000.0 { 500.0 } else { hz + 1.0 };
            freq.store(hz, core::sync::atomic::Ordering::Release);

            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

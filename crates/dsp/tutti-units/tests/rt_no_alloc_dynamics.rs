//! Regression gate: dynamics processors must not allocate per-buffer.
//!
//! Covers the four `tutti-units` dynamics nodes: `Compressor` (mono +
//! stereo), `Gate` (mono + stereo), `LimiterNode` (lookahead), and
//! `BrickwallLimiter` (zero-latency clipper).
//!
//! Lookahead limiter is the most failure-prone of the group — it carries
//! a monotonic deque + a ring buffer, both of which would historically
//! be tempting to resize on `set_lookahead`. The gate here covers steady
//! `process` only; lookahead changes are off-RT.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, ChannelLayout, SampleRate};
use tutti_units::{BrickwallLimiter, Compressor, Gate, LimiterNode};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn fill_with_signal(vec: &mut BufferVec, amplitude: f32) {
    let mut buf = vec.buffer_mut();
    let channels = buf.channels();
    for c in 0..channels {
        for i in 0..64 {
            // Alternating sign to keep the envelope follower active.
            let sample = if i % 2 == 0 { amplitude } else { -amplitude };
            buf.set_f32(c, i, sample);
        }
    }
}

#[test]
fn compressor_mono_process_is_allocation_free() {
    let mut node = Compressor::mono(-20.0, 4.0, 0.005, 0.050);
    node.set_sample_rate(SampleRate(48_000.0));

    // 2 inputs (audio + sidechain), 1 output.
    let mut input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(1);
    fill_with_signal(&mut input_vec, 0.7);

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
fn compressor_stereo_process_is_allocation_free() {
    let mut node = Compressor::stereo(-18.0, 3.0, 0.003, 0.080).with_soft_knee(6.0);
    node.set_sample_rate(SampleRate(48_000.0));

    // 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gain.
    let mut input_vec = BufferVec::new(4);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 0.8);

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
fn limiter_node_process_is_allocation_free() {
    let mut node = LimiterNode::new(-3.0, -0.3).with_lookahead(0.005);
    node.set_sample_rate(SampleRate(48_000.0));

    // 2 inputs (L/R), 2 outputs.
    let mut input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 1.5); // over ceiling

    // Warm up — fill the lookahead ring + prime the deque.
    for _ in 0..32 {
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
fn limiter_node_wide_6ch_process_is_allocation_free() {
    // The per-channel lookahead rings + frame scratch must be built at
    // construction; the linked-gain wide path must not allocate per buffer.
    let mut node =
        LimiterNode::with_channels(ChannelLayout::from_count(6), -3.0, -0.3).with_lookahead(0.005);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(6);
    let mut output_vec = BufferVec::new(6);
    fill_with_signal(&mut input_vec, 1.5);

    for _ in 0..32 {
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
fn brickwall_limiter_wide_6ch_process_is_allocation_free() {
    let mut node = BrickwallLimiter::with_channels(ChannelLayout::from_count(6), -0.3);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(6);
    let mut output_vec = BufferVec::new(6);
    fill_with_signal(&mut input_vec, 1.5);

    {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..5_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

#[test]
fn brickwall_limiter_process_is_allocation_free() {
    let mut node = BrickwallLimiter::new(-0.3);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 1.5);

    // Warm-up pass in its own scope so the buffer borrows end before the
    // measured loop re-borrows `input_vec` / `output_vec`.
    {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..5_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

#[test]
fn gate_mono_process_is_allocation_free() {
    let mut node = Gate::mono(-40.0, 0.001, 0.010, 0.100).with_range(-60.0);
    node.set_sample_rate(SampleRate(48_000.0));

    // 2 inputs (audio + sidechain), 1 output.
    let mut input_vec = BufferVec::new(2);
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
fn gate_stereo_process_is_allocation_free() {
    let mut node = Gate::stereo(-40.0, 0.001, 0.010, 0.100);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(4);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 0.6);

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

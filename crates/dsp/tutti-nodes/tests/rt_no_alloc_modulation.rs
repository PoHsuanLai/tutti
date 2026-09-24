//! Regression gate: modulation + delay nodes must not allocate per-buffer.
//!
//! Covers `LfoNode`, `DelayLineNode` (mono, stereo cross-fed, 6-wide), and
//! the modulation effects `ModDelayNode` (chorus and flanger) and `PhaserNode`. The big
//! shared risk is the delay-line allocation in `set_sample_rate` — that
//! call rebuilds the underlying `DelayLine` and must be invoked outside
//! the no-alloc gate.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, SampleRate};
use tutti_nodes::{DelayLineNode, LfoNode, LfoShape, ModDelayNode, PhaserNode};

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
fn lfo_node_process_is_allocation_free() {
    let mut node = LfoNode::new(LfoShape::Sine).with_frequency(2.0_f32);
    node.set_sample_rate(SampleRate(48_000.0));

    // 0 inputs, 1 output.
    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(1);

    for _ in 0..16 {
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
fn lfo_random_shape_process_is_allocation_free() {
    // RandomSmooth path has its own per-instance state machine.
    let mut node = LfoNode::new(LfoShape::RandomSmooth).with_frequency(4.0_f32);
    node.set_sample_rate(SampleRate(48_000.0));

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(1);

    for _ in 0..16 {
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
fn delay_line_node_process_is_allocation_free() {
    let mut node = DelayLineNode::new(2.0_f32, 0.25_f32, 0.4_f32);
    // set_sample_rate rebuilds the delay line — must run outside the gate.
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
fn stereo_delay_line_node_process_is_allocation_free() {
    let mut node = DelayLineNode::stereo(2.0_f32, 0.30_f32, 0.31_f32, 0.4_f32);
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
fn stereo_delay_line_node_wide_6ch_process_is_allocation_free() {
    // The per-channel delay-line Vec must be built at construction; the wide
    // path must not allocate per buffer.
    let mut node = DelayLineNode::with_channels(6usize, 2.0_f32, 0.30_f32, 0.4_f32);
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
fn chorus_node_process_is_allocation_free() {
    let mut node = ModDelayNode::chorus(tutti_core::ChannelLayout::STEREO);
    node.set_sample_rate(SampleRate(48_000.0));

    // Chorus is stereo in/out.
    let mut input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 0.4);

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
fn flanger_node_process_is_allocation_free() {
    let mut node = ModDelayNode::flanger(tutti_core::ChannelLayout::STEREO);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);
    fill_with_signal(&mut input_vec, 0.4);

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
fn phaser_node_process_is_allocation_free() {
    // 4-stage phaser. Construction allocates the all-pass Vec, but `process`
    // must reuse it.
    let mut node = PhaserNode::new(4);
    node.set_sample_rate(SampleRate(48_000.0));

    let mut input_vec = BufferVec::new(1);
    let mut output_vec = BufferVec::new(1);
    fill_with_signal(&mut input_vec, 0.4);

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
fn delay_and_modulation_with_moving_controls_are_allocation_free() {
    // Every control moved each block: the delay time glides, the mixes fade,
    // the phaser's coefficient table is re-solved, the cross-fed delay reads
    // every tap before writing any line. All of it on preallocated scratch.
    //
    // Mutation: a `Vec::with_capacity(width)` scratch in the phaser's
    // `render` fails.
    let mut delay = DelayLineNode::stereo(1.0_f32, 0.2_f32, 0.3_f32, 0.4_f32);
    delay.set_sample_rate(SampleRate(48_000.0));
    delay.set_cross_feedback(0.3);
    let mut chorus = ModDelayNode::chorus(6usize);
    chorus.set_sample_rate(SampleRate(48_000.0));
    let mut phaser = PhaserNode::with_channels(6usize, 8);
    phaser.set_sample_rate(SampleRate(48_000.0));

    let mut stereo_in = BufferVec::new(2);
    let mut stereo_out = BufferVec::new(2);
    let mut wide_in = BufferVec::new(6);
    let mut wide_out = BufferVec::new(6);
    fill_with_signal(&mut stereo_in, 0.5);
    fill_with_signal(&mut wide_in, 0.5);

    for _ in 0..16 {
        delay.process(64, &stereo_in.buffer_ref(), &mut stereo_out.buffer_mut());
        chorus.process(64, &wide_in.buffer_ref(), &mut wide_out.buffer_mut());
        phaser.process(64, &wide_in.buffer_ref(), &mut wide_out.buffer_mut());
    }

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            let t = (i % 50) as f32 / 50.0;
            delay.set_delay_time(0.05 + 0.4 * t);
            delay.set_mix(t);
            chorus.set_depth(0.002 + 0.01 * t);
            chorus.set_mix(1.0 - t);
            phaser.set_depth(t);
            phaser.set_feedback(0.9 * t);
            delay.process(64, &stereo_in.buffer_ref(), &mut stereo_out.buffer_mut());
            chorus.process(64, &wide_in.buffer_ref(), &mut wide_out.buffer_mut());
            phaser.process(64, &wide_in.buffer_ref(), &mut wide_out.buffer_mut());
        }
    });
}

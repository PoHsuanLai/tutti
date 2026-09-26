//! Regression gate: filter / EQ nodes must not allocate per-buffer.
//!
//! Covers `LadderFilterNode`, `EqBandNode`, `SvfFilterNode`, and
//! the wide `SvfFilterNode`. The shared risk is the coefficient cache: the
//! `maybe_update` hot path conditionally recomputes coeffs when params
//! change. A regression that recomputes every sample is still alloc-free
//! today, but a regression that *materialises* a coeff history (e.g. for
//! parameter smoothing) would land here.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, SampleRate};
use tutti_graph::contract::BlockRig;
use tutti_graph::IntoNode;
use tutti_nodes::{EqBandNode, LadderFilterNode, LadderType, SvfFilterNode, SvfType};

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

/// A native node through `BlockRig`: 16 warm-up blocks, then 2 000 blocks
/// under `assert_no_alloc`, `between` run before each (a control move).
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate(48_000.0), 64);
    // A 32-frame square: low enough that every filter here passes some of
    // it (a Nyquist square through a low-pass is silence, and the silence
    // check below would fail for the wrong reason).
    for c in rig.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if (i / 16) % 2 == 0 { 0.5 } else { -0.5 };
        }
    }
    for i in 0..16 {
        between(&controls, i);
        rig.block();
    }
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            between(&controls, i);
            rig.block();
        }
    });
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
    gate(
        EqBandNode::<f64>::new(SvfType::Bell, 1_000.0, 1.0, 6.0),
        |_, _| {},
    );
}

#[test]
fn svf_filter_mono_process_is_allocation_free() {
    gate(
        SvfFilterNode::<f64>::new(SvfType::LowPass, 800.0, 0.707),
        |_, _| {},
    );
}

#[test]
fn svf_filter_stereo_process_is_allocation_free() {
    gate(
        SvfFilterNode::<f64>::with_channels(
            tutti_core::ChannelLayout::STEREO,
            SvfType::HighPass,
            200.0,
            0.707,
        ),
        |_, _| {},
    );
}

#[test]
fn svf_filter_wide_6ch_process_is_allocation_free() {
    // The per-channel integrator Vec must be built at construction — a
    // regression that (re)allocated it per buffer, or per-channel scratch in
    // `process`, would trip here.
    gate(
        SvfFilterNode::<f64>::with_channels(6usize, SvfType::LowPass, 800.0, 0.707),
        |_, _| {},
    );
}

#[test]
fn ladder_filter_wide_6ch_process_is_allocation_free() {
    let mut node = LadderFilterNode::<f64>::with_channels(6usize, LadderType::LP24, 1_000.0, 0.6);
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
    // Exercises the moved-control branch: every block nudges the cutoff
    // (through the node's `ParamSet`, as a host sets it), forcing a
    // coefficient solve ramped across the block.
    let mut hz = 500.0f32;
    gate(
        SvfFilterNode::<f64>::new(SvfType::Bell, 1_000.0, 1.0),
        move |params, _| {
            hz = if hz > 4_000.0 { 500.0 } else { hz + 1.0 };
            params.set(tutti_core::UnitParam::Cutoff, hz);
        },
    );
}

#[test]
fn ladder_with_moving_cutoff_and_drive_is_allocation_free() {
    // A cutoff or drive that moved since the last block takes the ramped
    // path: a coefficient solve every 16 samples and a per-sample drive ramp.
    // Both must stay on the stack.
    //
    // Mutation: a `Vec::with_capacity(size)` scratch in `run_swept` fails.
    let mut node = LadderFilterNode::<f64>::with_channels(6usize, LadderType::LP24, 1_000.0, 0.6);
    node.set_sample_rate(SampleRate(48_000.0));
    let (freq, drive) = (node.frequency(), node.drive());

    let mut input_vec = BufferVec::new(6);
    let mut output_vec = BufferVec::new(6);
    fill_with_signal(&mut input_vec, 0.5);

    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        let mut hz = 500.0f32;
        for i in 0..2_000 {
            hz = if hz > 4_000.0 { 500.0 } else { hz + 7.0 };
            freq.store(hz, core::sync::atomic::Ordering::Release);
            drive.store(1.0 + (i % 7) as f32, core::sync::atomic::Ordering::Release);

            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

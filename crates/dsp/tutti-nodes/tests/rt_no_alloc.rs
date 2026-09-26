//! Regression gate for RT-safety: built-in nodes must not allocate on the
//! process path. The panner tests live in `tutti-spatial`'s own copy of this
//! gate, alongside the nodes they cover.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A native node through `BlockRig`, silent inputs: 16 warm-up blocks (past
/// the first FFT partition, so `process` takes the hot path), then 1 000
/// blocks under `assert_no_alloc`. Its `prepare` (which sizes the fold's
/// scratch) runs when the rig is built, outside the gate.
#[cfg(feature = "convolution")]
fn native_gate<N: tutti_graph::IntoNode>(node: N) {
    let (mut rig, _controls) =
        tutti_graph::contract::BlockRig::new(node, tutti_core::SampleRate(48_000.0), 64);
    for _ in 0..16 {
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            rig.block();
        }
    });
}

#[cfg(feature = "convolution")]
#[test]
fn convolver_process_is_allocation_free() {
    use tutti_nodes::{generate_test_ir, ConvolverNode};

    let ir = generate_test_ir(2048, 0.3, 48_000.0);
    native_gate(ConvolverNode::new(&ir, 512));
}

#[cfg(feature = "convolution")]
#[test]
fn stereo_convolver_process_is_allocation_free() {
    use tutti_nodes::{generate_test_ir, ConvolverNode};

    let ir_l = generate_test_ir(2048, 0.3, 48_000.0);
    let ir_r = generate_test_ir(2048, 0.4, 48_000.0);
    native_gate(ConvolverNode::stereo(&ir_l, &ir_r, 512));
}

/// The fold path is the one with scratch: a width-long frame buffer and a
/// block-long mono buffer, both sized before the first block (the frame at
/// construction, the mono block at `prepare`), never grown in `process`. Six
/// channels, so the fold does real work rather than the stereo average.
///
/// Mutation (run): building the mono block as a `vec!` inside `render`
/// fails the no-alloc gate.
#[cfg(feature = "convolution")]
#[test]
fn folded_six_channel_convolver_process_is_allocation_free() {
    use tutti_nodes::{generate_test_ir, ConvolverNode};

    let irs: Vec<Vec<f32>> = (0..6)
        .map(|c| generate_test_ir(1024, 0.2 + 0.05 * c as f32, 48_000.0))
        .collect();
    let refs: Vec<&[f32]> = irs.iter().map(Vec::as_slice).collect();
    native_gate(ConvolverNode::folded(&refs, 256));
}

/// `DownmixNode::process` gathers an interleaved frame per sample and folds it.
/// Both scratch buffers are sized at construction and taken with `mem::take`
/// precisely so that path never allocates — this is the test that makes those
/// two fields load-bearing rather than incidental. Driven natively, alone in a
/// graph (`BlockRig`).
///
/// 6→2 is the shape that matters: it is the only one where the fold does real
/// work (an equal-width or upmix fold is a copy), and it is what a 5.1 bus
/// feeding a stereo master looks like.
///
/// Mutation (run): build `frame` with `vec!` inside `process` instead of
/// taking the scratch → fails.
#[test]
fn downmix_process_is_allocation_free() {
    // Imported here rather than at file scope: the module-level imports are
    // gated on `convolution`, and this node needs it not.
    use tutti_core::{ChannelLayout, SampleRate};
    use tutti_graph::contract::BlockRig;
    use tutti_nodes::DownmixNode;

    let node = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
    let (mut rig, ()) = BlockRig::new(node, SampleRate(48_000.0), 64);
    for (c, x) in rig.inputs_mut().iter_mut().enumerate() {
        x.fill(0.1 * (c + 1) as f32);
    }

    // Warm up: first-touch faults must not be counted against the RT path.
    rig.block();
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the fold rendered silence; the gate would walk no DSP"
    );

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            rig.block();
        }
    });
}

//! Regression gate: the crossover must not allocate per block, including
//! while a crossover frequency moves.
//!
//! The node runs alone in a graph through `tutti_graph::contract::BlockRig`.
//! The moving-control gate takes the ramped path (a coefficient solve every
//! few samples).
//!
//! Mutation (run): build the node's `scratch` in `process` rather than
//! `prepare` (`self.scratch = vec![0.0; ..]` at its top) → both gates fail
//! on the first measured block; a `Vec` built in `split_block`'s ramp → the
//! moving gate fails.

use assert_no_alloc::AllocDisabler;
use tutti_core::{ChannelLayout, Hz, SampleRate};
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::CrossoverNode;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `node` through `BlockRig`: a 32-frame square (broadband, so every band is
/// driven), 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`,
/// `between` run before each.
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate(48_000.0), 64);
    for c in rig.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if (i / 16) % 2 == 0 { 0.8 } else { -0.8 };
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
fn crossover_node_process_is_allocation_free() {
    let node = CrossoverNode::new(
        ChannelLayout::STEREO,
        &[Hz(120.0), Hz(1_000.0), Hz(6_000.0)],
    )
    .unwrap();
    gate(node, |_, _| {});
}

#[test]
fn crossover_node_with_a_moving_frequency_is_allocation_free() {
    let node = CrossoverNode::new(ChannelLayout::STEREO, &[Hz(200.0), Hz(3_000.0)]).unwrap();
    let mut hz = 200.0f32;
    gate(node, move |controls, _| {
        hz = if hz > 2_000.0 { 200.0 } else { hz + 3.0 };
        controls.set_frequency(0, Hz(hz)).unwrap();
    });
}

//! Regression gate: the crossover and the multiband compressor must not
//! allocate per block, including while a crossover frequency, a threshold or a
//! solo moves.
//!
//! Each node runs alone in a graph through `tutti_graph::contract::BlockRig`.
//! The moving-control gates take the ramped path (a coefficient solve every
//! few samples), a solo's mute ramp and a bypass.
//!
//! Mutation (run): size the multiband node's `split` scratch in `process`
//! rather than `prepare` (`self.split = vec![0.0; ..]` at its top) → every
//! gate on it fails on the first measured block; build the crossover's
//! `scratch` in `process` → the crossover gates fail.

use assert_no_alloc::AllocDisabler;
use tutti_core::{ChannelLayout, Hz, SampleRate};
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{CrossoverNode, MultibandCompressorNode};

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

#[test]
fn multiband_compressor_process_is_allocation_free() {
    for bands in 2..=4 {
        let node = MultibandCompressorNode::new(ChannelLayout::STEREO, bands).unwrap();
        let c = node.controls();
        for b in 0..bands {
            let band = c.band(b).unwrap();
            band.set_threshold(-24.0);
            band.set_ratio(4.0);
            band.set_makeup(3.0);
        }
        gate(node, |_, _| {});
    }
}

#[test]
fn multiband_compressor_with_moving_controls_is_allocation_free() {
    let node = MultibandCompressorNode::new(ChannelLayout::from_count(6), 4).unwrap();
    let mut hz = 120.0f32;
    gate(node, move |controls, i| {
        hz = if hz > 400.0 { 120.0 } else { hz + 1.0 };
        controls.set_crossover(0, Hz(hz)).unwrap();
        let band = controls.band(i % 4).unwrap();
        band.set_threshold(-30.0 + (i % 11) as f32);
        band.set_makeup((i % 5) as f32);
        band.set_bypass(i % 7 == 0);
        band.set_solo(i % 13 == 0);
        controls.band((i + 1) % 4).unwrap().set_solo(false);
    });
}

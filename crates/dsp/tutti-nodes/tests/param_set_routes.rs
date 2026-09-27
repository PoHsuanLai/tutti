//! A control-rate modulation route onto a node's param resolves on the
//! node's [`ParamSet`](tutti_graph::ParamSet): the host takes the set's cell
//! for the param and mirrors it through an [`AtomicTarget`] (bevy-tutti's
//! `ParamSetTargets`), so a route writes exactly the cell the node reads.
//!
//! These pinned each node's `ModParams` impl (`src/mod_params.rs`) while a
//! route resolved through a registry of node types; the registry and the
//! impls went with `Legacy` (doc 013, "Legacy deleted"), and the `ParamSet`
//! is each node's one address. Each assertion is the old one, asked of the
//! set. Two changed with it, both deliberate:
//!
//! - a foreign `ParamAddr::Id` is no longer expressible: a set is addressed
//!   by `UnitParam` alone (the old `native_node_ignores_a_foreign_id`);
//! - the compressor's makeup is `GainDb`, its one address, where the
//!   `ModParams` impl answered `Makeup` for the same cell (one cell, one
//!   address; see the CHANGELOG).
//!
//! Mutation (run): `ParamSet::cell` answering every param with the set's
//! first cell → every test asking for a param the set lacks, or one that is
//! not its first, fails (5 of 7); the two that route the SVF's cutoff, its
//! first param, pass, as they must.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tutti_core::{ChannelLayout, UnitParam};
use tutti_graph::ParamNode;
use tutti_mod::{AtomicTarget, LayerKey, ModTarget};
use tutti_nodes::{
    CompressorNode, LadderFilterNode, LadderType, ModDelayNode, SvfFilterNode, SvfType,
};

/// The route a host resolves for `p` on `node`: its `ParamSet` cell behind
/// an `AtomicTarget` over `[min, max]`, or `None` for a param it lacks.
fn route<N: ParamNode>(
    node: &N,
    p: UnitParam,
    base: f32,
    min: f32,
    max: f32,
) -> Option<Arc<dyn ModTarget>> {
    node.param_set()
        .cell(p)
        .map(|cell| Arc::new(AtomicTarget::with_mirror(base, min, max, cell)) as Arc<dyn ModTarget>)
}

fn svf() -> SvfFilterNode<f32> {
    SvfFilterNode::<f32>::with_channels(ChannelLayout::STEREO, SvfType::LowPass, 1000.0, 0.7)
}

#[test]
fn filter_cutoff_route_moves_the_nodes_own_cell() {
    let node = svf();
    let atomic = node.frequency(); // the node reads this per block
    let target =
        route(&node, UnitParam::Cutoff, 1000.0, 20.0, 20000.0).expect("cutoff is modulatable");

    // Accumulating an offset moves the node's OWN cell.
    target.accumulate(LayerKey(1), 500.0);
    assert!(
        (atomic.load(Ordering::Acquire) - 1500.0).abs() < 1e-3,
        "the node's frequency cell reflects the modulation"
    );
    target.clear(LayerKey(1));
    assert!((atomic.load(Ordering::Acquire) - 1000.0).abs() < 1e-3);
}

#[test]
fn a_param_the_set_lacks_has_no_route() {
    // The SVF has no Drive param.
    assert!(route(&svf(), UnitParam::Drive, 0.0, 0.0, 1.0).is_none());
}

#[test]
fn ladder_routes_q_and_drive_but_not_feedback() {
    let node =
        LadderFilterNode::<f32>::with_channels(ChannelLayout::STEREO, LadderType::LP24, 800.0, 0.5);
    assert!(route(&node, UnitParam::Cutoff, 800.0, 20.0, 20000.0).is_some());
    assert!(route(&node, UnitParam::Q, 0.5, 0.0, 1.0).is_some());
    assert!(route(&node, UnitParam::Drive, 1.0, 0.0, 4.0).is_some());
    assert!(route(&node, UnitParam::Feedback, 0.0, 0.0, 1.0).is_none());
}

// ── End-to-end: modulate a REAL audio node through the ModMatrix ──

#[test]
fn lfo_sweeps_a_real_filter_cutoff_through_the_matrix() {
    use tutti_core::{Beat, BeatDuration, Seconds};
    use tutti_mod::{Lfo, LfoShape, ModMatrix, SourceRate};

    // A real filter node. The node reads `frequency()` per block.
    let filter = svf();
    let cutoff_atomic = filter.frequency();

    // Resolve the node's cutoff route and hand it to the matrix.
    let target =
        route(&filter, UnitParam::Cutoff, 1000.0, 20.0, 20000.0).expect("cutoff is modulatable");

    let mut m = ModMatrix::new();
    let cutoff = m.add_target(target); // register the node's OWN cell
                                       // Beat-synced at 1 cycle/beat → phase == beat, so passing beat = i/16
                                       // walks a full sine cycle.
    m.route(
        Lfo::new(LfoShape::Sine),
        SourceRate::beat_synced(BeatDuration(1.0), 0.0),
    )
    .to(&cutoff)
    .depth(0.5);
    let mut driver = m.build();

    // Drive the matrix each frame → the filter's real cutoff cell sweeps.
    let mut moved_up = false;
    let mut moved_down = false;
    for i in 0..16 {
        driver.run(Beat(i as f64 / 16.0), Seconds(0.0));
        let hz = cutoff_atomic.load(Ordering::Acquire);
        assert!(
            (20.0..=20000.0).contains(&hz),
            "cutoff left its range: {hz}"
        );
        if hz > 1000.5 {
            moved_up = true;
        }
        if hz < 999.5 {
            moved_down = true;
        }
    }
    assert!(
        moved_up && moved_down,
        "a sine LFO should sweep the cutoff both ways"
    );
}

#[test]
fn chorus_rate_route_moves_the_nodes_own_cell() {
    let node = ModDelayNode::chorus(ChannelLayout::STEREO);
    let rate_atomic = node.rate();
    let base = rate_atomic.load(Ordering::Acquire);
    let target =
        route(&node, UnitParam::Rate, base, 0.01, 10.0).expect("chorus rate is modulatable");

    target.accumulate(LayerKey(1), 2.0);
    assert!(
        (rate_atomic.load(Ordering::Acquire) - (base + 2.0)).abs() < 1e-3,
        "chorus rate cell reflects the modulation"
    );
    target.clear(LayerKey(1));
    assert!((rate_atomic.load(Ordering::Acquire) - base).abs() < 1e-3);

    // A param it doesn't expose has no route.
    assert!(route(&node, UnitParam::Cutoff, 0.0, 0.0, 1.0).is_none());
}

/// The compressor's makeup has one address, `GainDb`; `Makeup` (what its
/// `ModParams` impl answered for the same cell) is not in its set.
#[test]
fn compressor_makeup_is_addressed_as_gain_db() {
    let node = CompressorNode::with_channels(
        tutti_core::Db(-12.0),
        tutti_core::CompressionRatio(4.0),
        tutti_core::Seconds(0.01),
        tutti_core::Seconds(0.1),
        2,
    );
    let makeup = node.makeup_gain();
    let target = route(&node, UnitParam::GainDb, 0.0, -24.0, 24.0).expect("makeup is GainDb");
    target.accumulate(LayerKey(1), 6.0);
    assert!((makeup.load(Ordering::Acquire) - 6.0).abs() < 1e-3);
    assert!(route(&node, UnitParam::Makeup, 0.0, -24.0, 24.0).is_none());
}

#[cfg(feature = "convolution")]
#[test]
fn convolver_only_routes_wet() {
    // Room size is baked into the IR; only Wet is control-rate modulatable.
    // A minimal unit IR is fine — only the param surface is probed.
    let node = tutti_nodes::ConvolverNode::shared_ir(2usize, &[1.0], 64);
    assert!(route(&node, UnitParam::Wet, 0.5, 0.0, 1.0).is_some());
    assert!(route(&node, UnitParam::RoomSize, 0.5, 0.0, 1.0).is_none());
    assert!(route(&node, UnitParam::Cutoff, 0.5, 0.0, 1.0).is_none());
}

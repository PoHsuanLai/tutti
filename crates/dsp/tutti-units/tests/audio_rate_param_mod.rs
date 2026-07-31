//! Spike: what does wiring an audio-rate param modulation edge actually cost,
//! expressed in plain `Net` calls with no ECS?
//!
//! This is the baseline any declarative wrapper has to beat.

use std::sync::Arc;

use tutti_core::dsp::{AudioUnit, Net};
use tutti_core::{AtomicF32, Ordering};
use tutti_mod::{CurveType, Polarity};
use tutti_types::{Depth, UnitParam};
use tutti_units::{
    AtomicSourceUnit, DistortionNode, ParamPorts, ParamShaperUnit, ParamSumUnit, ShapeKind,
};

/// Wire `base + shaped(source) → node.param_port(param)`.
///
/// The whole audio-rate edge, tutti-only. Returns the shared base atomic so a
/// control thread can move the authored value.
fn wire_param_mod(
    net: &mut Net,
    target: tutti_core::NodeId,
    port: usize,
    source: tutti_core::NodeId,
    authored: f32,
    range: (f32, f32),
    depth: Depth,
) -> Arc<AtomicF32> {
    let base_unit = AtomicSourceUnit::new(authored);
    let base_cell = base_unit.shared();

    let base = net.push(Box::new(base_unit));
    let shaper = net.push(Box::new(ParamShaperUnit::new(
        depth,
        Polarity::Bipolar,
        CurveType::Linear,
    )));
    let sum = net.push(Box::new(ParamSumUnit::new(1, range.0, range.1)));

    net.connect(base, 0, sum, 0); // port 0 = base
    net.connect(source, 0, shaper, 0);
    net.connect(shaper, 0, sum, 1); // ports 1..=N = offsets
    net.connect(sum, 0, target, port);

    base_cell
}

/// A constant source standing in for an LFO, so the test asserts on arithmetic
/// rather than on a waveform's phase.
fn constant(net: &mut Net, v: f32) -> tutti_core::NodeId {
    let unit = AtomicSourceUnit::new(v);
    net.push(Box::new(unit))
}

#[test]
fn the_whole_edge_is_six_net_calls() {
    let mut net = Net::new(2, 2);

    // A distortion born with its drive port on: 3 inputs (L, R, drive).
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let port = dist.param_port(UnitParam::Drive).expect("drive port");
    assert_eq!(port, 2, "the param port follows the audio inputs");
    let target = net.push(Box::new(dist));

    let source = constant(&mut net, 1.0); // full-positive modulation
    let base = wire_param_mod(
        &mut net,
        target,
        port,
        source,
        5.0,
        (0.0, 10.0),
        Depth::FULL,
    );

    net.pipe_output(target);
    net.check();

    // The base cell is live: a control thread moves the authored value and the
    // graph sees it without any rebuild.
    base.store(7.0, Ordering::Release);
    assert_eq!(base.load(Ordering::Acquire), 7.0);
}

/// The property that makes the two tiers compose: the audio-rate sum's base
/// port is fed from the *same atomic* a control-rate `AtomicTarget` mirrors
/// into. One base, both tiers.
#[test]
fn control_rate_and_audio_rate_share_one_base_cell() {
    let shared = Arc::new(AtomicF32::new(5.0));

    // The control-rate tier owns this cell (an AtomicTarget mirrors into it).
    // The audio-rate tier reads the same cell as its base.
    let base_unit = AtomicSourceUnit::over(Arc::clone(&shared));
    let mut net = Net::new(0, 1);
    let base = net.push(Box::new(base_unit));
    net.pipe_output(base);
    net.check();

    // A control-rate flush moves the cell...
    shared.store(8.0, Ordering::Release);

    // ...and the audio-rate base sees it, with no second accumulator involved.
    let mut out = [0.0f32; 1];
    net.tick(&[], &mut out);
    assert_eq!(out[0], 8.0, "the audio-rate base is the control-rate cell");
}

/// `ParamSumUnit` is what makes fan-in representable: `Net` holds one source
/// per input port, so summing N modulation edges has to be a node.
#[test]
fn sum_folds_base_plus_n_offsets_and_clamps() {
    let mut sum = ParamSumUnit::new(2, 0.0, 10.0);

    let mut out = [0.0f32; 1];
    sum.tick(&[5.0, 1.5, 2.0], &mut out);
    assert_eq!(out[0], 8.5, "base + Σ offsets");

    // The clamp is the sum's, applied once after folding — not per edge.
    sum.tick(&[9.0, 3.0, 4.0], &mut out);
    assert_eq!(out[0], 10.0, "one clamp, at the top of the range");

    sum.tick(&[1.0, -5.0, -3.0], &mut out);
    assert_eq!(out[0], 0.0, "and at the bottom");
}

/// The shaper is the same `tutti_mod::shape` the control-rate path applies, so
/// both tiers agree on values. Baking it to a LUT is a performance decision,
/// not a second implementation.
#[test]
fn shaper_agrees_with_the_control_rate_shaping_function() {
    let depth = Depth(0.5);
    let shaper = ParamShaperUnit::new(depth, Polarity::Bipolar, CurveType::Linear);

    for x in [-1.0f32, -0.5, -0.25, 0.0, 0.25, 0.5, 1.0] {
        let mut out = [0.0f32; 1];
        let mut s = shaper.clone();
        s.tick(&[x], &mut out);
        let expected = tutti_mod::shape(x, depth, Polarity::Bipolar, CurveType::Linear);
        assert!(
            (out[0] - expected).abs() < 1e-3,
            "LUT diverged from shape() at {x}: {} vs {expected}",
            out[0]
        );
    }
}

/// **The hazard a declarative wrapper would have to solve.**
///
/// Param ports are ordinary input ports that happen to sit after the audio
/// inputs — so the bulk helpers do not know to leave them alone.
/// `pipe_input` walks *every* input port of the target, which silently
/// overwrites a param edge that `connect` already claimed.
///
/// There is no error and no warning: the modulation simply stops arriving, and
/// the param reads whatever the global input carries (`Zero` here). This is
/// exactly the "one writer per port" rule `bevy_tutti`'s `wire.rs` enforces for
/// audio — and nothing enforces it here.
#[test]
fn param_port_is_clobbered_by_pipe_input() {
    let mut net = Net::new(1, 1);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));

    let base = net.push(Box::new(AtomicSourceUnit::new(9.0)));
    let sum = net.push(Box::new(ParamSumUnit::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, port);

    // Everything is wired correctly at this point...
    let wired = net.source(target, port);
    assert_eq!(
        wired,
        tutti_core::dsp::Source::Local(sum, 0),
        "the param edge exists before pipe_input"
    );

    // ...and this one call, which reads as "wire the audio in", destroys it.
    net.pipe_input(target);

    assert_ne!(
        net.source(target, port),
        wired,
        "pipe_input silently overwrote the param edge — it walks ALL input ports"
    );
}

/// End to end: the modulation actually reaches the node's param port and moves
/// the sound. Without the edge the drive sits at the authored value.
#[test]
fn the_edge_changes_what_the_node_produces() {
    fn render(with_modulation: bool) -> Vec<f32> {
        let mut net = Net::new(1, 1);
        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
        let port = dist.param_port(UnitParam::Drive).unwrap();
        let target = net.push(Box::new(dist));

        let drive = if with_modulation { 9.0 } else { 1.0 };
        let base = net.push(Box::new(AtomicSourceUnit::new(drive)));
        let sum = net.push(Box::new(ParamSumUnit::new(0, 0.0, 10.0)));
        net.connect(base, 0, sum, 0);
        net.connect(sum, 0, target, port);

        // Wire the audio input *explicitly*, port by port. `pipe_input` would
        // overwrite EVERY input port — including the param port `connect` just
        // claimed — silently reverting the modulation edge to a global input.
        // See `param_port_is_clobbered_by_pipe_input`.
        net.connect_input(0, target, 0);
        net.pipe_output(target);
        net.check();

        (0..64)
            .map(|i| {
                let mut out = [0.0f32; 1];
                net.tick(&[(i as f32 * 0.1).sin() * 0.8], &mut out);
                out[0]
            })
            .collect()
    }

    let clean = render(false);
    let driven = render(true);
    let diff: f32 = clean.iter().zip(&driven).map(|(a, b)| (a - b).abs()).sum();

    assert!(
        diff > 1.0,
        "driving through the param port must change the output; total diff {diff}"
    );
}

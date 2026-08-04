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
    AtomicSourceUnit, DistortionNode, ParamModShaping, ParamPorts, ParamShaperUnit, ParamSumUnit,
    ShapeKind,
};

/// Wire `base + shaped(source) → node.param_port(param)`.
///
/// A single-edge shim over the crate's own [`tutti_units::wire_param_mod`],
/// kept so these tests read as they did when this was a local helper. That the
/// six tests below pass **unmodified** against the promoted version is the
/// proof the extraction preserved behaviour.
fn wire_param_mod(
    net: &mut Net,
    target: tutti_core::NodeId,
    port: usize,
    source: tutti_core::NodeId,
    authored: f32,
    range: (f32, f32),
    depth: Depth,
) -> Arc<AtomicF32> {
    tutti_units::wire_param_mod(
        net,
        target,
        port,
        authored,
        range.0,
        range.1,
        &[(
            source,
            ParamModShaping {
                depth,
                polarity: Polarity::Bipolar,
                curve: CurveType::Linear,
            },
        )],
    )
    .base_cell()
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

/// **The cell the builder hands back is the one the sum actually reads.**
///
/// The whole value of returning a handle is that a control thread can move the
/// authored value *after* the chain is built. If the returned `Arc` were not
/// the sum's own cell — a fresh atomic, a clone of a snapshot — this would
/// still compile, still render, and the param would sit frozen at its
/// construction value forever.
///
/// That is not a hypothetical failure mode. It is what `bevy-tutti`'s
/// `spawn_chain` did, and it presented as "the cutoff knob does nothing".
#[test]
fn the_builder_returns_the_cell_the_sum_actually_reads() {
    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).expect("drive port");
    let target = net.push(Box::new(dist));

    // No modulation at all: the base is the entire signal, so any movement in
    // the output is unambiguously the base moving.
    let chain = tutti_units::wire_param_mod(&mut net, target, port, 1.0, 0.0, 10.0, &[]);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);
    net.pipe_output(target);
    net.check();

    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);
    let quiet = out[0];

    chain
        .base_cell()
        .store(9.0, std::sync::atomic::Ordering::Release);
    net.tick(&[0.5, 0.5], &mut out);

    assert!(
        out[0] > quiet + 0.3,
        "a write to the returned cell must reach the node: {quiet} -> {}. \
         An unchanged output means the handle is not the cell the sum reads.",
        out[0]
    );
}

/// **The builder's cell composes with a control-rate accumulator.**
///
/// `sharing_one_cell_makes_control_rate_and_audio_rate_compose` in
/// `param_writer_ownership` proves this with a hand-built `Arc`. This proves
/// the same thing through the *public* API, which is what a host actually
/// reaches for — the composition is only useful if the assembler exposes the
/// cell that makes it possible.
#[test]
fn the_builders_cell_is_the_cell_a_control_rate_target_mirrors_into() {
    use tutti_mod::{AtomicTarget, ModTarget};

    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).expect("drive port");
    let target = net.push(Box::new(dist));

    let chain = tutti_units::wire_param_mod(&mut net, target, port, 1.0, 0.0, 10.0, &[]);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);
    net.pipe_output(target);
    net.check();

    // The control-rate tier, mirroring into the audio-rate chain's base.
    let acc = AtomicTarget::with_mirror(1.0, 0.0, 10.0, chain.base_cell());

    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);
    let quiet = out[0];

    // An authored move through the control-rate sink's own vocabulary.
    acc.set_base(9.0);
    net.tick(&[0.5, 0.5], &mut out);

    assert!(
        out[0] > quiet + 0.3,
        "`set_base` on an accumulator mirroring the chain's base cell must \
         reach the node: {quiet} -> {}",
        out[0]
    );
}

/// **Offsets land on ports `1..=N`; the base keeps port 0.**
///
/// An off-by-one here would put the first shaper on the base port, so the
/// authored value would be replaced by a modulation offset rather than added
/// to — audible as a param that ignores its knob and swings around zero.
#[test]
fn n_edges_land_on_ports_one_through_n() {
    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).expect("drive port");
    let target = net.push(Box::new(dist));

    let a = constant(&mut net, 1.0);
    let b = constant(&mut net, 1.0);
    let edge = |source| {
        (
            source,
            ParamModShaping {
                depth: Depth(0.25),
                polarity: Polarity::Bipolar,
                curve: CurveType::Linear,
            },
        )
    };
    let chain =
        tutti_units::wire_param_mod(&mut net, target, port, 1.0, 0.0, 10.0, &[edge(a), edge(b)]);

    assert_eq!(chain.shapers.len(), 2, "one shaper per edge");
    assert_eq!(
        net.source(chain.sum, 0),
        tutti_core::dsp::Source::Local(chain.base, 0),
        "port 0 must stay the base"
    );
    for (i, &shaper) in chain.shapers.iter().enumerate() {
        assert_eq!(
            net.source(chain.sum, i + 1),
            tutti_core::dsp::Source::Local(shaper, 0),
            "shaper {i} must occupy port {}",
            i + 1
        );
    }
}

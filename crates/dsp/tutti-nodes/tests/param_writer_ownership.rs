//! Who owns a param's value, once an audio-rate port exists?
//!
//! A param can be written from three places: the node's own atomic (the
//! authored value), a control-rate accumulator mirroring into that same atomic,
//! and — once a param port is wired — an audio-rate signal arriving on the port.
//!
//! These pin which one the node actually reads, because the answer decides
//! where an authored fader move has to land.

use std::sync::Arc;

use tutti_core::dsp::Net;
use tutti_core::AudioUnit as _;
use tutti_core::{AtomicF32, Ordering};
use tutti_nodes::{AtomicSourceNode, DistortionNode, ParamPorts, ParamSumNode, ShapeKind};
use tutti_types::UnitParam;

/// Render one sample of `dist` fed a constant, reporting the output.
/// Saturation is monotonic in drive, so the output is a proxy for "what drive
/// did the node actually use".
fn render(net: &mut Net, target: tutti_core::NodeId) -> f32 {
    net.pipe_output(target);
    net.check();
    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);
    out[0]
}

/// **The port wins outright.** With a param port wired, the node never reads its
/// atomic — `process` takes the drive from `input.at_f32(drive_port, i)` and the
/// atomic is not consulted at all.
///
/// This is what makes the control-rate `set_base` path *wrong* for an
/// audio-rate-modulated param: that path writes the accumulator, which mirrors
/// into the atomic, which nothing reads. The authored value would silently stop
/// having any effect.
#[test]
fn a_wired_param_port_makes_the_node_ignore_its_atomic() {
    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).unwrap();
    // The handle a control-rate AtomicTarget would mirror into.
    let authored = dist.drive();
    let target = net.push(Box::new(dist));

    // Port carries a high drive; the atomic says something else entirely.
    let base = net.push(Box::new(AtomicSourceNode::new(9.0)));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, port);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);

    let with_port = render(&mut net, target);

    // Move the atomic the "authored value" path writes. If the node read it,
    // this would change the output.
    authored.store(0.1, Ordering::Release);
    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);

    assert!(
        (out[0] - with_port).abs() < 1e-6,
        "writing the atomic changed the output ({with_port} -> {}), but a wired \
         param port is supposed to override it entirely",
        out[0]
    );
    assert!(
        with_port.abs() > 0.9,
        "the port's drive of 9.0 should saturate hard; got {with_port}"
    );
}

/// The corollary: with the port wired, the authored value has to reach the
/// **base cell of the sum**, not the node's atomic.
///
/// This is the single-writer rule one level out — the same rule that already
/// sends an authored value to a control-rate accumulator's base instead of the
/// node. Audio-rate just moves the base one more hop.
#[test]
fn the_authored_value_must_land_on_the_sums_base_cell() {
    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));

    let base_unit = AtomicSourceNode::new(1.0);
    let base_cell = base_unit.shared();
    let base = net.push(Box::new(base_unit));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, port);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);

    let quiet = render(&mut net, target);

    // An authored move written to the SUM'S BASE does reach the node.
    base_cell.store(9.0, Ordering::Release);
    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);

    assert!(
        out[0] > quiet + 0.3,
        "an authored write to the sum's base must reach the node: {quiet} -> {}",
        out[0]
    );
}

/// The composition that makes all three writers coherent: point the sum's base
/// cell **at the same atomic** a control-rate accumulator mirrors into.
///
/// Then there is still exactly one base, and the control-rate tier keeps
/// working unchanged — its flush lands in the cell, and the cell is what the
/// audio-rate sum adds its offsets onto. No arbitration, no third writer.
#[test]
fn sharing_one_cell_makes_control_rate_and_audio_rate_compose() {
    // The cell a control-rate AtomicTarget mirrors into.
    let shared = Arc::new(AtomicF32::new(1.0));

    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));

    // The audio-rate base reads that very cell.
    let base = net.push(Box::new(AtomicSourceNode::over(Arc::clone(&shared))));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, port);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);

    let quiet = render(&mut net, target);

    // A control-rate flush (base + Σ control-rate offsets) lands in the cell...
    shared.store(9.0, Ordering::Release);
    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);

    assert!(
        out[0] > quiet + 0.3,
        "the control-rate cell must still drive the node through the audio-rate \
         base: {quiet} -> {}",
        out[0]
    );
}

/// **A private base cell cannot be moved by a control-rate write.**
///
/// The negative of `sharing_one_cell_makes_control_rate_and_audio_rate_compose`
/// above, and the reason [`AtomicSourceNode::over`] exists. Build the chain's
/// base with `new()` — a cell only the chain can see — and a control-rate
/// accumulator mirroring into *its own* cell has nowhere to land.
///
/// Nothing errors. The graph is valid, the node renders, and the authored value
/// is simply frozen at whatever it was when the chain was built. That is the
/// failure mode this file exists to make unmissable: it presented downstream as
/// "the cutoff knob does nothing".
#[test]
fn a_private_base_cell_cannot_be_moved_by_a_control_rate_write() {
    use tutti_mod::{AtomicTarget, ModTarget};

    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));

    // The chain's base is a private cell: `new`, and the handle is dropped.
    let base = net.push(Box::new(AtomicSourceNode::new(1.0)));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, port);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);

    let quiet = render(&mut net, target);

    // A control-rate accumulator over a *different* cell — which is what an
    // unshared chain leaves you with.
    let elsewhere = Arc::new(AtomicF32::new(1.0));
    let acc = AtomicTarget::with_mirror(1.0, 0.0, 10.0, Arc::clone(&elsewhere));
    acc.set_base(9.0);

    let mut out = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut out);

    assert_eq!(
        elsewhere.load(Ordering::Acquire),
        9.0,
        "the accumulator did flush — the write is not the thing that failed"
    );
    assert!(
        (out[0] - quiet).abs() < 1e-6,
        "with a private base cell the node cannot see the control-rate write, \
         yet the output moved ({quiet} -> {}). If this fails, the two cells \
         are somehow shared and the test no longer proves what it claims.",
        out[0]
    );
}

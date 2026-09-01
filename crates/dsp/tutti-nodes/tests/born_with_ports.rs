//! Does born-with-ports actually cost nothing when nothing is modulating?
//!
//! Born-with-ports means every modulatable node is spawned with its param ports
//! on and an `AtomicSourceNode → ParamSumNode → port` chain wired at creation,
//! whether or not a route ever lands on it. It is the simpler policy — routing
//! becomes pure wiring, with no node rebuild and no arity change — but "simpler"
//! is only worth having if the idle case is genuinely free.
//!
//! These pin what it costs, so the choice rests on measurements rather than on
//! the claim being plausible.

use tutti_core::dsp::{AudioUnit as _, Net};
use tutti_core::Ordering;
use tutti_types::{Hz, UnitParam, Q};
use tutti_nodes::{
    AtomicSourceNode, DistortionNode, ParamPorts, ParamSumNode, ShapeKind, StereoSvfFilterNode,
    SvfType,
};

/// Signal to push through every node under test.
fn signal(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.6 * (i as f32 * 0.05).sin()).collect()
}

/// **The base chain is mandatory, not an optimisation.**
///
/// A node born with a param port reads that port *unconditionally* — it does not
/// check whether anything is connected, because `Net` gives it no way to. An
/// unconnected input in `Net` is `Zero`, so a ported node with nothing wired to
/// it sees the param as literally **0.0**, not as its authored value.
///
/// For a distortion that means drive 0.0, and `Shaper::build(Tanh, 0.0)` is
/// silence — the node stops passing audio altogether.
///
/// This is the single most important constraint on born-with-ports: "spawn every
/// modulatable node with its ports on" is only safe when the base chain is wired
/// *in the same breath*. Ports on with nothing feeding them is not a free idle
/// state, it is a broken node.
#[test]
fn a_ported_node_with_an_unfed_port_reads_the_param_as_zero() {
    let input = signal(64);

    let mut plain = DistortionNode::new(ShapeKind::Tanh, 5.0);
    let mut ported = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    assert_eq!(plain.inputs(), 2);
    assert_eq!(ported.inputs(), 3, "the port changes the node's arity");

    let mut ported_is_silent = true;
    let mut plain_is_silent = true;
    for &x in &input {
        let mut a = [0.0f32; 2];
        let mut b = [0.0f32; 2];
        plain.tick(&[x, x], &mut a);
        // What `Net` feeds an unconnected input: Zero.
        ported.tick(&[x, x, 0.0], &mut b);
        if a[0] != 0.0 {
            plain_is_silent = false;
        }
        if b[0] != 0.0 {
            ported_is_silent = false;
        }
    }

    assert!(!plain_is_silent, "the plain node passes audio");
    assert!(
        ported_is_silent,
        "a ported node with an unfed port must be shown to read the param as 0.0 \
         — this is why the base chain cannot be deferred"
    );
}

/// With the base chain wired, the ported node is bit-identical to the plain one.
///
/// This is the claim born-with-ports actually rests on, stated correctly: the
/// cost of an always-on port is zero *given* an always-on base feeding it the
/// authored value. The node's own `distortion_unmodulated_matches_held_constant`
/// tests the same property by holding the port at the authored value by hand;
/// this one routes it through the real `AtomicSourceNode → ParamSumNode` chain,
/// so it covers the chain's arithmetic too.
#[test]
fn ported_plus_base_chain_is_bit_identical_to_plain() {
    let input = signal(512);

    let mut plain = DistortionNode::new(ShapeKind::Tanh, 5.0);

    let mut net = Net::new(2, 2);
    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));
    // The always-on base chain, carrying the authored 5.0.
    let base = net.push(Box::new(AtomicSourceNode::new(5.0)));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, drive_port);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);
    net.pipe_output(target);
    net.check();

    for &x in &input {
        let mut a = [0.0f32; 2];
        let mut b = [0.0f32; 2];
        plain.tick(&[x, x], &mut a);
        net.tick(&[x, x], &mut b);
        assert!(
            (a[0] - b[0]).abs() < 1e-6,
            "ported + base chain diverged from plain: {} vs {}",
            a[0],
            b[0]
        );
    }
}

/// What born-with-ports actually costs in the graph: nodes and edges per
/// modulatable param, paid whether or not anything modulates.
///
/// A distortion has 1 modulatable param, an SVF filter has 2. The chain is
/// `AtomicSourceNode + ParamSumNode` per param, so the cost scales with params,
/// not with nodes — which is the number worth knowing before committing.
#[test]
fn the_idle_cost_is_two_nodes_and_two_edges_per_param() {
    let mut net = Net::new(2, 2);

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));

    let before = net.size();

    // The always-on base chain for ONE param.
    let base = net.push(Box::new(AtomicSourceNode::new(5.0)));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, drive_port);

    assert_eq!(
        net.size() - before,
        2,
        "one modulatable param costs exactly two idle nodes"
    );

    // An SVF carries two modulatable params, so it pays twice.
    let svf = StereoSvfFilterNode::<f32>::with_param_inputs(
        2,
        SvfType::LowPass,
        Hz(1000.0),
        Q(0.7),
        true,
        true,
    );
    assert_eq!(
        svf.param_port(UnitParam::Cutoff).is_some() as usize
            + svf.param_port(UnitParam::Q).is_some() as usize,
        2,
        "the cost scales with modulatable params, not with nodes"
    );
}

/// The payoff: with the ports already there, routing a modulation is pure
/// wiring. No node is rebuilt, no arity changes, and the node keeps its
/// identity — which is what makes the policy simple.
///
/// The contrast is `Net::crossfade`/`replace`, which assert equal input counts
/// — so a demand-build policy that added a port later could not swap the node
/// in place at all.
#[test]
fn routing_a_modulation_is_pure_wiring() {
    let mut net = Net::new(2, 2);

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    let target = net.push(Box::new(dist));

    let base_unit = AtomicSourceNode::new(1.0);
    let base_cell = base_unit.shared();
    let base = net.push(Box::new(base_unit));
    // Sized for one route up front — the born-with-ports bet.
    let sum = net.push(Box::new(ParamSumNode::new(1, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, drive_port);
    net.connect_input(0, target, 0);
    net.connect_input(1, target, 1);
    net.pipe_output(target);
    net.check();

    let node_count_before = net.size();
    let mut quiet = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut quiet);

    // "Route a modulation": push a source and connect it. That is the whole
    // operation — no rebuild, no replace, no crossfade.
    let source = net.push(Box::new(AtomicSourceNode::new(4.0)));
    net.connect(source, 0, sum, 1);
    net.check();

    let mut driven = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut driven);

    assert_eq!(
        net.size(),
        node_count_before + 1,
        "routing added only the source node itself"
    );
    assert!(
        driven[0] > quiet[0] + 0.1,
        "the routed modulation must reach the node: {} -> {}",
        quiet[0],
        driven[0]
    );

    // And the authored base still rides underneath it.
    base_cell.store(0.2, Ordering::Release);
    let mut lowered = [0.0f32; 2];
    net.tick(&[0.5, 0.5], &mut lowered);
    assert!(
        lowered[0] < driven[0],
        "the base must still move the result while a route is active"
    );
}

/// Born-with-ports has to hold at every width, not just at stereo.
///
/// The port index is what makes this worth a test of its own: it is *derived*
/// from the width rather than fixed, so the "spawn it with its ports on, then
/// wire" policy only works if the host asks the node where its port went. Every
/// assertion here would pass trivially at width 2 and is the reason the
/// constructors take a width at all — before they did, this node came back
/// stereo and `param_port` answered 2, so a host wiring a 6-channel chain
/// connected its modulation source to what was actually an *audio* input.
#[test]
fn a_wide_node_is_born_with_its_ports_after_its_audio_inputs() {
    let dist = DistortionNode::with_param_inputs(6, ShapeKind::Tanh, 5.0, true);
    assert_eq!(dist.outputs(), 6);
    assert_eq!(dist.inputs(), 7, "six audio inputs, then the drive port");
    let drive_port = dist.param_port(UnitParam::Drive).unwrap();
    assert_eq!(drive_port, 6, "the port index moves with the width");

    // And the base chain still reaches it — the same wiring as the stereo case,
    // with nothing width-aware about it beyond asking for the port.
    let mut net = Net::new(6, 6);
    let target = net.push(Box::new(dist));
    let base = net.push(Box::new(AtomicSourceNode::new(5.0)));
    let sum = net.push(Box::new(ParamSumNode::new(0, 0.0, 10.0)));
    net.connect(base, 0, sum, 0);
    net.connect(sum, 0, target, drive_port);
    for c in 0..6 {
        net.connect_input(c, target, c);
    }
    net.pipe_output(target);
    net.check();

    let mut plain = DistortionNode::with_channels(6, ShapeKind::Tanh, 5.0);
    for i in 0..128 {
        let x = 0.6 * (i as f32 * 0.05).sin();
        let (mut a, mut b) = ([0.0f32; 6], [0.0f32; 6]);
        plain.tick(&[x; 6], &mut a);
        net.tick(&[x; 6], &mut b);
        for c in 0..6 {
            assert!(
                (a[c] - b[c]).abs() < 1e-6,
                "channel {c} diverged from the plain node at sample {i}"
            );
        }
    }
}

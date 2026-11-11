//! Width genericity: an N-channel node is N independent mono nodes.
//!
//! The SVF, the ladder, the delay (uncoupled, so wider than stereo), the
//! chorus (with no phase stagger) and the phaser have no cross-channel
//! coupling. So an N-channel node of one setting, fed N different inputs,
//! renders on each channel exactly what a one-channel node of the same setting
//! renders from that channel's input alone, whatever moves the controls: a
//! held cell, a cell moved between blocks, or a lane on a param port, across a
//! ragged block pattern. The check is bit for bit: the channels share a
//! coefficient table and a block's ramps, and nothing may mix them.

use tutti_graph::contract::{drive, prepared};
use tutti_graph::{Node, ParamNode, ParamSet};
use tutti_nodes::{
    DelayLineNode, InterpolationMode, LadderFilterNode, LadderType, ModDelayNode, PhaserNode,
    SvfFilterNode, SvfType,
};
use tutti_types::{
    ChannelLayout, Feedback, Hz, PhaseIncrement, Q, Resonance, SampleRate, Seconds, UnitParam,
};

const SR: SampleRate = SampleRate(48_000.0);
const LEN: usize = 2_048;
const WIDTH: u16 = 6;
/// Ragged on purpose: full blocks, odd sizes, and a block of one.
const PATTERN: [usize; 6] = [64, 64, 17, 1, 64, 33];

/// What moves a node's controls: lanes on its param ports (`None`: nothing fed
/// there) and a write to its cells before each block.
struct Moves {
    lanes: Vec<Option<Vec<f32>>>,
    cells: Option<Box<dyn Fn(&ParamSet, usize)>>,
}

impl Moves {
    fn held() -> Self {
        Self {
            lanes: Vec::new(),
            cells: None,
        }
    }

    fn lanes(lanes: Vec<Option<Vec<f32>>>) -> Self {
        Self { lanes, cells: None }
    }

    fn cells(f: impl Fn(&ParamSet, usize) + 'static) -> Self {
        Self {
            lanes: Vec::new(),
            cells: Some(Box::new(f)),
        }
    }
}

/// A ramp `from` to `to` over the render, per frame.
fn ramp(from: f32, to: f32) -> Vec<f32> {
    (0..LEN)
        .map(|i| from + (to - from) * (i as f32 / LEN as f32))
        .collect()
}

/// An exponential sweep `from` to `to` over the render, per frame.
fn sweep(from: f32, to: f32) -> Vec<f32> {
    (0..LEN)
        .map(|i| from * (to / from).powf(i as f32 / LEN as f32))
        .collect()
}

fn render<N: Node + ParamNode + 'static>(
    node: N,
    inputs: &[Vec<f32>],
    moves: &Moves,
) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, 64);
    let set = node.param_set();
    let mut out: Vec<Vec<f32>> = Vec::new();
    let (mut pos, mut k) = (0, 0);
    while pos < LEN {
        let n = PATTERN[k % PATTERN.len()].min(LEN - pos);
        if let Some(cells) = &moves.cells {
            cells(&set, k);
        }
        k += 1;
        let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[pos..pos + n]).collect();
        let lanes: Vec<Option<&[f32]>> = moves
            .lanes
            .iter()
            .map(|l| l.as_ref().map(|l| &l[pos..pos + n]))
            .collect();
        let block = drive(&mut node as &mut dyn Node, SR, &ins, &lanes);
        out.resize(block.len(), Vec::with_capacity(LEN));
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
        pos += n;
    }
    out
}

/// Every channel of the wide node is the mono node of that channel's input.
fn assert_width_generic<N: Node + ParamNode + 'static>(
    what: &str,
    make: impl Fn(ChannelLayout) -> N,
    moves: Moves,
) {
    let inputs: Vec<Vec<f32>> = (0..WIDTH)
        .map(|c| tutti_fixtures::noise(u32::from(c) + 1, LEN))
        .collect();
    let wide = render(make(ChannelLayout::from(WIDTH)), &inputs, &moves);
    assert_eq!(wide.len(), usize::from(WIDTH), "{what}: width");
    for (c, input) in inputs.iter().enumerate() {
        let mono = render(
            make(ChannelLayout::MONO),
            std::slice::from_ref(input),
            &moves,
        );
        assert!(
            wide[c].iter().any(|s| s.abs() > 1e-4),
            "{what}: channel {c} is silent, the comparison would be empty"
        );
        let same = wide[c]
            .iter()
            .zip(&mono[0])
            .all(|(a, b)| a.to_bits() == b.to_bits());
        assert!(same, "{what}: channel {c} differs from the mono render");
    }
}

fn svf(layout: ChannelLayout) -> SvfFilterNode<f64> {
    SvfFilterNode::<f64>::with_channels(layout, SvfType::LowPass, Hz::new(1_000.0), Q::new(0.9))
}

fn ladder(layout: ChannelLayout) -> LadderFilterNode<f64> {
    LadderFilterNode::<f64>::with_channels(
        layout,
        LadderType::LP24,
        Hz::new(900.0),
        Resonance::new(0.6),
    )
}

fn delay(layout: ChannelLayout) -> DelayLineNode {
    DelayLineNode::with_channels(
        layout,
        Seconds::new(0.5),
        Seconds::new(0.0123),
        Feedback::new(0.55),
    )
    .with_interpolation(InterpolationMode::CubicHermite)
}

fn chorus(layout: ChannelLayout) -> ModDelayNode {
    // No stagger: one phase for every channel, as the mono node has.
    let zeros = vec![PhaseIncrement(0.0); usize::from(layout.count())];
    let node = ModDelayNode::chorus(layout).with_phase_offsets(&zeros);
    node.set_rate(Hz::new(1.7));
    node.set_depth(Seconds::new(0.004));
    node.set_feedback(Feedback::new(0.3));
    node
}

fn phaser(layout: ChannelLayout) -> PhaserNode {
    let node = PhaserNode::with_channels(layout, 6);
    node.set_rate(Hz::new(2.0));
    node.set_feedback(Feedback::new(0.5));
    node
}

/// The SVF's channels are independent whether its cutoff is held, swept by a
/// lane or moved between blocks.
#[test]
fn svf_channels_are_independent_mono_filters() {
    assert_width_generic("svf held", svf, Moves::held());
    assert_width_generic(
        "svf swept",
        svf,
        Moves::lanes(vec![Some(sweep(200.0, 8_000.0)), None]),
    );
    assert_width_generic(
        "svf automated",
        svf,
        Moves::cells(|set, k| {
            assert!(set.set(UnitParam::Cutoff, 300.0 + 700.0 * (k % 5) as f32));
        }),
    );
}

/// The ladder's channels are independent whether its cutoff is held, swept by a
/// lane or its drive moved between blocks.
#[test]
fn ladder_channels_are_independent_mono_filters() {
    assert_width_generic("ladder held", ladder, Moves::held());
    assert_width_generic(
        "ladder swept",
        ladder,
        Moves::lanes(vec![Some(sweep(200.0, 8_000.0)), None, None]),
    );
    assert_width_generic(
        "ladder drive automated",
        ladder,
        Moves::cells(|set, k| {
            assert!(set.set(UnitParam::Drive, 1.0 + (k % 4) as f32));
        }),
    );
}

/// An uncoupled delay's channels are independent whether its feedback and time
/// are held or on lanes, or its feedback is moved between blocks. (The param
/// set addresses channel 0's time cell alone, so a time written there is not
/// every channel's; a time lane is.)
#[test]
fn delay_channels_are_independent_mono_delays() {
    assert_width_generic("delay held", delay, Moves::held());
    assert_width_generic(
        "delay lanes",
        delay,
        Moves::lanes(vec![Some(ramp(0.0, 0.9)), Some(ramp(0.002, 0.012))]),
    );
    assert_width_generic(
        "delay automated",
        delay,
        Moves::cells(|set, k| {
            assert!(set.set(UnitParam::Feedback, 0.1 * (k % 8) as f32));
        }),
    );
}

/// An unstaggered chorus's channels are independent whether its controls are
/// held, on lanes, or moved between blocks.
#[test]
fn chorus_channels_are_independent_mono_choruses() {
    assert_width_generic("chorus held", chorus, Moves::held());
    assert_width_generic(
        "chorus lanes",
        chorus,
        Moves::lanes(vec![
            Some(sweep(0.5, 4.0)),
            Some(ramp(0.001, 0.006)),
            Some(ramp(0.0, 0.5)),
        ]),
    );
    assert_width_generic(
        "chorus automated",
        chorus,
        Moves::cells(|set, k| {
            assert!(set.set(UnitParam::Rate, 0.5 + 0.5 * (k % 4) as f32));
            assert!(set.set(UnitParam::Depth, 0.001 + 0.001 * (k % 3) as f32));
        }),
    );
}

/// The phaser's channels are independent whether its controls are held, on
/// lanes, or moved between blocks.
#[test]
fn phaser_channels_are_independent_mono_phasers() {
    assert_width_generic("phaser held", phaser, Moves::held());
    assert_width_generic(
        "phaser lanes",
        phaser,
        Moves::lanes(vec![
            Some(sweep(0.3, 5.0)),
            Some(ramp(0.2, 1.0)),
            Some(ramp(0.0, 0.7)),
        ]),
    );
    assert_width_generic(
        "phaser automated",
        phaser,
        Moves::cells(|set, k| {
            assert!(set.set(UnitParam::Depth, 0.2 * (k % 5) as f32));
            assert!(set.set(UnitParam::Feedback, 0.1 * (k % 6) as f32));
        }),
    );
}

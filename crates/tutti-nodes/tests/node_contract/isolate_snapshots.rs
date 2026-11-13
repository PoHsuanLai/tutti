//! Every forkable node in this crate forks to a **snapshot** of its controls:
//! no live control move reaches the fork. One row per node, one control per
//! cell the node reads; `tutti_graph::contract::IsolateRow` runs each control
//! through fork → render → move live → render again (must be unchanged) → fork
//! again → render (must differ, so the control is audible and the row can
//! fail). A node's fork is `fork_fresh` (`tutti_graph::param_parts`), so every
//! cell a control writes is checked — addressed by its `ParamSet` or not (a
//! gate's hold) — with the fork taken as the graph takes
//! it.
//!
//! Units with no live cell are not rows — there is nothing to move:
//! `AutomationLaneNode` (its `Arc<dyn Curve>`
//! is read-only, or frozen by its fork when it is not — its own unit test
//! `a_fork_freezes_a_live_curve`) and the `testing` stimulus nodes.

use tutti_graph::ParamNode;
use tutti_graph::contract::IsolateRow;
use tutti_nodes::{
    BrickwallLimiterNode, BusStripNode, ChannelTimes, CompressorControls, CompressorNode,
    DelayControls, DelayFamily, DelayNode, DistortionNode, EqBandNode, GateControls, GateNode,
    InterpolationMode, KeySource, LadderFilterNode, LadderType, LfoNode, LfoShape, LimiterControls,
    LimiterNode, MaxDelay, ModDelayNode, PhaserControls, PhaserNode, ShapeKind, Stages,
    SvfControls, SvfFilterNode, SvfType,
};
use tutti_types::{
    Amplitude, BusTopology, ChannelLayout, CompressionRatio, Db, Depth, Drive, Feedback, Hz, Q,
    Resonance, Seconds, UnitParam,
};

#[test]
fn compressor() {
    IsolateRow::new("CompressorNode (stereo)", || {
        CompressorNode::new(
            BusTopology::STEREO,
            KeySource::Sidechain,
            CompressorControls {
                threshold: Db(-20.0),
                knee: Db(3.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds(0.005),
                release: Seconds(0.05),
                makeup: Db(2.0),
            },
        )
    })
    .control("threshold", |c| {
        assert!(c.param_set().set(UnitParam::Threshold, -30.0));
    })
    .control("ratio", |c| {
        assert!(c.param_set().set(UnitParam::Ratio, 10.0));
    })
    .control("attack", |c| {
        assert!(c.param_set().set(UnitParam::Attack, 0.02));
    })
    .control("release", |c| {
        assert!(c.param_set().set(UnitParam::Release, 0.2));
    })
    .control("makeup", |c| {
        assert!(c.param_set().set(UnitParam::GainDb, 6.0));
    })
    .control("knee", |c| {
        assert!(c.param_set().set(UnitParam::Knee, 9.0));
    })
    .check();
}

#[test]
fn gate() {
    IsolateRow::new("GateNode (stereo)", || {
        GateNode::new(
            BusTopology::STEREO,
            KeySource::Sidechain,
            GateControls {
                threshold: Db(-15.0),
                attack: Seconds(0.001),
                hold: Seconds(0.005),
                release: Seconds(0.02),
                range: Db(-40.0),
            },
        )
    })
    .control("threshold", |g| {
        assert!(g.param_set().set(UnitParam::Threshold, -35.0));
    })
    .control("attack", |g| {
        assert!(g.param_set().set(UnitParam::Attack, 0.01));
    })
    .control("hold", |g| {
        assert!(g.param_set().set(UnitParam::Hold, 0.05));
    })
    .control("release", |g| {
        assert!(g.param_set().set(UnitParam::Release, 0.1));
    })
    .control("range", |g| {
        assert!(g.param_set().set(UnitParam::Range, -10.0));
    })
    .check();
}

#[test]
fn limiter() {
    IsolateRow::new("LimiterNode (stereo)", || {
        LimiterNode::stereo(LimiterControls {
            threshold: Db(-6.0),
            ceiling: Db(-3.0),
            ..LimiterControls::DEFAULT
        })
    })
    .control("threshold", |l| {
        assert!(l.param_set().set(UnitParam::Threshold, -12.0));
    })
    .control("ceiling", |l| {
        assert!(l.param_set().set(UnitParam::Ceiling, -9.0));
    })
    .control("release", |l| {
        assert!(l.param_set().set(UnitParam::Release, 0.3));
    })
    .check();
}

#[test]
fn brickwall_limiter() {
    IsolateRow::new("BrickwallLimiterNode (stereo)", || {
        BrickwallLimiterNode::new(Db(-3.0))
    })
    .control("ceiling", |l| {
        assert!(l.param_set().set(UnitParam::Ceiling, -9.0));
    })
    .check();
}

#[test]
fn delay_line() {
    IsolateRow::new("DelayNode (stereo, ping-pong)", || {
        let family = DelayFamily::new(
            BusTopology::STEREO,
            MaxDelay::of(Seconds(1.0)),
            InterpolationMode::Linear,
        )
        .with_times(ChannelTimes::each(&[Seconds(0.07)]).unwrap())
        .unwrap();
        DelayNode::live(
            family,
            DelayControls {
                time: Seconds(0.05),
                feedback: Feedback::new(0.5_f32),
                cross_feedback: Feedback::new(0.3_f32),
            },
        )
    })
    .control("delay time", |d| {
        assert!(d.param_set().set(UnitParam::DelayTime, 0.08));
    })
    .control("feedback", |d| {
        assert!(d.param_set().set(UnitParam::Feedback, 0.1));
    })
    .control("cross feedback", |d| {
        assert!(d.param_set().set(UnitParam::CrossFeedback, 0.0));
    })
    .check();
}

#[test]
fn distortion() {
    IsolateRow::new("DistortionNode (stereo)", || {
        DistortionNode::new(ShapeKind::Tanh, Drive(2.0))
    })
    .control("drive", |d| {
        assert!(d.param_set().set(UnitParam::Drive, 8.0))
    })
    .check();
}

#[test]
fn lfo() {
    IsolateRow::new("LfoNode (sine, 3 Hz)", || {
        LfoNode::new(LfoShape::Sine)
            .with_rate(Hz(3.0))
            .with_depth(Depth::clamped(0.8))
    })
    .control("rate", |l| assert!(l.param_set().set(UnitParam::Rate, 5.0)))
    .control("depth", |l| {
        assert!(l.param_set().set(UnitParam::Depth, 0.3))
    })
    .check();
}

#[test]
fn chorus() {
    IsolateRow::new("ModDelayNode (stereo chorus)", || {
        ModDelayNode::chorus(BusTopology::STEREO)
    })
    .control("rate", |c| assert!(c.param_set().set(UnitParam::Rate, 3.0)))
    .control("depth", |c| {
        assert!(c.param_set().set(UnitParam::Depth, 0.001))
    })
    .control("feedback", |c| {
        assert!(c.param_set().set(UnitParam::Feedback, 0.6));
    })
    .check();
}

#[test]
fn phaser() {
    IsolateRow::new("PhaserNode (stereo, 4 stages)", || {
        PhaserNode::stereo(
            Stages::of(4),
            PhaserControls {
                feedback: Feedback::new(0.3_f32),
                ..PhaserControls::DEFAULT
            },
        )
    })
    .control("rate", |p| assert!(p.param_set().set(UnitParam::Rate, 3.0)))
    .control("depth", |p| {
        assert!(p.param_set().set(UnitParam::Depth, 0.2))
    })
    .control("feedback", |p| {
        assert!(p.param_set().set(UnitParam::Feedback, 0.7));
    })
    .check();
}

#[test]
fn ladder() {
    IsolateRow::new("LadderFilterNode (LP12)", || {
        LadderFilterNode::<f64>::new(LadderType::LP12, Hz(1_000.0), Resonance::new_clamped(0.3))
    })
    .control("cutoff", |l| {
        assert!(l.param_set().set(UnitParam::Cutoff, 3_000.0));
    })
    .control("resonance", |l| {
        assert!(l.param_set().set(UnitParam::Q, 0.8));
    })
    .control("drive", |l| {
        assert!(l.param_set().set(UnitParam::Drive, 4.0));
    })
    .check();
}

/// The SVF and the EQ band are graph nodes: their fork is
/// `tutti_graph::param_parts`', from the values set through their
/// `ParamSet`, and `assert_param_fork` is its snapshot check (their own
/// unit tests run it).
#[test]
fn svf() {
    tutti_graph::contract::assert_param_fork(SvfFilterNode::<f64>::with_controls(
        ChannelLayout::MONO,
        SvfControls {
            ty: SvfType::Bell,
            freq: Hz(1_000.0),
            q: Q(0.7),
            gain: Db(6.0),
        },
    ));
}

#[test]
fn eq_band() {
    tutti_graph::contract::assert_param_fork(EqBandNode::<f64>::new(
        SvfType::Bell,
        Hz(1_000.0),
        Q(0.7),
        Db(6.0),
    ));
}

#[test]
fn bus_strip() {
    IsolateRow::new("BusStripNode (stereo)", BusStripNode::stereo)
        .control("volume", |s| s.set_volume(Amplitude(0.5)))
        .control("pan", |s| assert!(s.param_set().set(UnitParam::Pan, -0.7)))
        .check();
}
#[test]
fn convolver() {
    use tutti_nodes::ConvolverNode;
    // A short decaying tail: a fixed LCG, so the IR is the same every run.
    let ir: Vec<f32> = (0..512u32)
        .map(|n| {
            let r = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) >> 8;
            let white = r as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            white * (-(n as f32) / 128.0).exp()
        })
        .collect();
    IsolateRow::new("ConvolverNode (mono)", move || ConvolverNode::new(&ir, 64))
        .control("gain", |c| c.set_gain(Amplitude(0.3)))
        .check();
}

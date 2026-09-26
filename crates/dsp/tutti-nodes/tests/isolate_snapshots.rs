//! Every forkable unit in this crate forks to a **snapshot** of its
//! controls: after `clone` + `isolate`, no live control move reaches the
//! fork (doc 013, gap 6's audit). One row per unit, one control per cell
//! the unit reads; `tutti_graph::contract::IsolateRow` runs each control
//! through fork → render → move live → render again (must be unchanged) →
//! fork again → render (must differ, so the control is audible and the row
//! can fail).
//!
//! Units with no live cell are not rows — there is nothing to move:
//! `ChannelSumNode`, `DownmixNode`, `AutomationLaneNode` (its `Arc<dyn Curve>`
//! is read-only, or frozen by its fork when it is not — its own unit test
//! `a_fork_freezes_a_live_curve`; `set_curve` takes `&mut self` and so cannot
//! reach a live node) and the `testing` stimulus nodes.
//!
//! Native nodes' rows are a `NativeRow` (below): the same check, over
//! `ParamNode::fork_fresh`.
//!
//! Mutations (run): delete any one `detach` line from a unit's `isolate`
//! (or the whole `isolate`) → that unit's row fails on exactly that
//! control, with "a live move reached the fork". Make `Param::detach`
//! reset to `U::default()` instead of keeping the value → rows whose
//! default differs fail. Make a control's write a no-op → that control
//! fails with "moving it did not change a fresh fork's output".

use tutti_core::{
    Amplitude, ChannelLayout, CompressionRatio, Db, Depth, Drive, Hz, Pan, PhaseIncrement,
    Resonance, Seconds, Q,
};
use tutti_graph::contract::IsolateRow;
use tutti_nodes::{
    BrickwallLimiterNode, BusStripNode, CompressorNode, DelayLineNode, DistortionNode, EqBandNode,
    GateNode, LadderFilterNode, LadderType, LfoNode, LfoShape, LimiterNode, ModDelayNode,
    PhaserNode, ShapeKind, SvfFilterNode, SvfType,
};

#[test]
fn compressor() {
    IsolateRow::new("CompressorNode (stereo)", || {
        CompressorNode::stereo(Db(-20.0), 4.0, Seconds(0.005), Seconds(0.05))
            .with_soft_knee(Db(3.0))
            .with_makeup(Db(2.0))
    })
    .control("threshold", |c| c.set_threshold(Db(-30.0)))
    .control("ratio", |c| {
        c.set_ratio(CompressionRatio::new_clamped(10.0))
    })
    .control("attack", |c| c.set_attack(Seconds(0.02)))
    .control("release", |c| c.set_release(Seconds(0.2)))
    .control("makeup", |c| c.set_makeup(Db(6.0)))
    .control("knee", |c| {
        c.knee_width().store(9.0, tutti_core::Ordering::Release)
    })
    .check();
}

#[test]
fn gate() {
    IsolateRow::new("GateNode (stereo)", || {
        GateNode::stereo(Db(-15.0), Seconds(0.001), Seconds(0.005), Seconds(0.02))
            .with_range(Db(-40.0))
    })
    .control("threshold", |g| g.set_threshold(Db(-35.0)))
    .control("attack", |g| g.set_attack(Seconds(0.01)))
    .control("hold", |g| {
        g.hold_time().store(0.05, tutti_core::Ordering::Release)
    })
    .control("release", |g| g.set_release(Seconds(0.1)))
    .control("range", |g| {
        g.range().store(-10.0, tutti_core::Ordering::Release)
    })
    .check();
}

#[test]
fn limiter() {
    IsolateRow::new("LimiterNode (stereo)", || {
        LimiterNode::new(Db(-6.0), Db(-3.0))
    })
    .control("threshold", |l| l.set_threshold(Db(-12.0)))
    .control("ceiling", |l| l.set_ceiling(Db(-9.0)))
    .control("release", |l| l.set_release(Seconds(0.3)))
    .check();
}

#[test]
fn brickwall_limiter() {
    IsolateRow::new("BrickwallLimiterNode (stereo)", || {
        BrickwallLimiterNode::new(Db(-3.0))
    })
    .control("ceiling", |l| {
        l.ceiling().store(-9.0, tutti_core::Ordering::Release)
    })
    .check();
}

/// A native node's form of an `IsolateRow`: its fork is
/// `ParamNode::fork_fresh` (what `tutti_graph::param_parts`' fork runs, before
/// it applies the authored values), and each control is every cell the node
/// reads, addressed or not. For each: fork → move the control live → the fork
/// renders what an unmoved node's fork renders; fork again → it renders
/// something else, so the control is audible and the row can fail. The
/// addressed cells' authored values are `assert_param_fork`'s, in each node's
/// own unit tests.
///
/// Mutations (run): delete any one `detach` from a native node's
/// `fork_fresh` → that node's row fails on exactly the controls behind that
/// cell, with "a live move reached the fork" (`DelayLineNode`'s
/// `cross_feedback`, `LfoNode`'s `phase_offset`, the convolver's `params`).
/// Make a control's setter a no-op → "moving it did not change a fresh
/// fork's output".
struct NativeRow<N> {
    name: &'static str,
    make: Box<dyn Fn() -> N>,
    controls: Vec<(&'static str, Box<dyn Fn(&N)>)>,
}

impl<N: tutti_graph::ParamNode> NativeRow<N> {
    fn new(name: &'static str, make: impl Fn() -> N + 'static) -> Self {
        Self {
            name,
            make: Box::new(make),
            controls: Vec::new(),
        }
    }

    fn control(mut self, name: &'static str, f: impl Fn(&N) + 'static) -> Self {
        self.controls.push((name, Box::new(f)));
        self
    }

    /// `node` over deterministic noise, one 8192-frame block at 48 kHz.
    fn render(node: N) -> Vec<Vec<f32>> {
        const FRAMES: usize = 8_192;
        let mut d =
            tutti_graph::contract::Direct::new(node, tutti_core::SampleRate(48_000.0), FRAMES);
        let mut state = 0x9E37_79B9_u32;
        for c in d.inputs_mut() {
            for x in c.iter_mut() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *x = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            }
        }
        d.block();
        let outs = usize::from(tutti_graph::Node::shape(&d.node).audio_out.count());
        (0..outs).map(|c| d.output(c).to_vec()).collect()
    }

    fn check(self) {
        let name = self.name;
        let unmoved = Self::render((self.make)().fork_fresh());
        for (control, moved) in &self.controls {
            let live = (self.make)();
            let fork = live.fork_fresh();
            moved(&live);
            assert!(
                Self::render(fork) == unmoved,
                "{name}: {control}: a live move reached the fork"
            );
            assert!(
                Self::render(live.fork_fresh()) != unmoved,
                "{name}: {control}: moving it did not change a fresh fork's output"
            );
        }
    }
}

#[test]
fn delay_line() {
    NativeRow::new("DelayLineNode (stereo, ping-pong)", || {
        let d = DelayLineNode::stereo(Seconds(1.0), Seconds(0.05), Seconds(0.07), 0.5_f32);
        d.set_cross_feedback(0.3_f32);
        d.set_mix(0.5_f32);
        d
    })
    .control("delay time (left)", |d| d.set_delay_time_l(Seconds(0.08)))
    .control("delay time (right)", |d| d.set_delay_time_r(Seconds(0.03)))
    .control("feedback", |d| d.set_feedback(0.1_f32))
    .control("cross feedback", |d| d.set_cross_feedback(0.0_f32))
    .control("mix", |d| d.set_mix(0.9_f32))
    .check();
}

#[test]
fn distortion() {
    IsolateRow::new("DistortionNode (stereo)", || {
        DistortionNode::new(ShapeKind::Tanh, Drive(2.0))
    })
    .control("drive", |d| d.set_drive(Drive(8.0)))
    .check();
}

#[test]
fn lfo() {
    NativeRow::new("LfoNode (sine, 3 Hz)", || {
        LfoNode::new(LfoShape::Sine)
            .with_frequency(Hz(3.0))
            .with_depth(Depth::new_clamped(0.8))
    })
    .control("frequency", |l| l.set_frequency(Hz(5.0)))
    .control("depth", |l| l.set_depth(Depth::new_clamped(0.3)))
    .control("phase offset", |l| l.set_phase_offset(PhaseIncrement(0.25)))
    .check();
}

#[test]
fn chorus() {
    NativeRow::new("ModDelayNode (stereo chorus)", || {
        let c = ModDelayNode::chorus(ChannelLayout::STEREO);
        c.set_feedback(0.3_f32);
        c
    })
    .control("rate", |c| c.set_rate(Hz(3.0)))
    .control("depth", |c| c.set_depth(Seconds(0.001)))
    .control("feedback", |c| c.set_feedback(0.6_f32))
    .control("mix", |c| c.set_mix(0.2_f32))
    .check();
}

#[test]
fn phaser() {
    NativeRow::new("PhaserNode (stereo, 4 stages)", || {
        let p = PhaserNode::with_channels(ChannelLayout::STEREO, 4);
        p.set_feedback(0.3_f32);
        p
    })
    .control("rate", |p| p.set_rate(Hz(3.0)))
    .control("depth", |p| p.set_depth(Depth::new_clamped(0.2)))
    .control("feedback", |p| p.set_feedback(0.7_f32))
    .control("mix", |p| p.set_mix(0.2_f32))
    .check();
}

#[test]
fn ladder() {
    IsolateRow::new("LadderFilterNode (LP12)", || {
        LadderFilterNode::<f64>::new(LadderType::LP12, Hz(1_000.0), Resonance::new_clamped(0.3))
    })
    .control("cutoff", |l| l.set_frequency(Hz(3_000.0)))
    .control("resonance", |l| {
        l.set_resonance(Resonance::new_clamped(0.8))
    })
    .control("drive", |l| l.set_drive(Drive(4.0)))
    .check();
}

/// The SVF and the EQ band are native nodes: their fork is
/// `tutti_graph::param_parts`', from the values set through their
/// `ParamSet`, and `assert_param_fork` is its snapshot check (their own
/// unit tests run it, with its mutations).
#[test]
fn svf() {
    tutti_graph::contract::assert_param_fork(
        SvfFilterNode::<f64>::new(SvfType::Bell, Hz(1_000.0), Q(0.7)).with_gain_db(Db(6.0)),
    );
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
    IsolateRow::new("BusStripNode (stereo)", BusStripNode::new)
        .control("volume", |s| s.set_volume(Amplitude(0.5)))
        .control("pan", |s| s.set_pan(Pan::new_clamped(-0.7)))
        .control("mute", |s| s.set_muted(true))
        .check();
}

#[cfg(feature = "convolution")]
#[test]
fn convolver() {
    use tutti_core::Mix;
    use tutti_nodes::ConvolverNode;
    // A short decaying tail: a fixed LCG, so the IR is the same every run.
    let ir: Vec<f32> = (0..512u32)
        .map(|n| {
            let r = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) >> 8;
            let white = r as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            white * (-(n as f32) / 128.0).exp()
        })
        .collect();
    NativeRow::new("ConvolverNode (mono)", move || {
        let c = ConvolverNode::new(&ir, 64);
        c.set_mix(Mix::new_clamped(0.5));
        c
    })
    .control("mix", |c| c.set_mix(Mix::new_clamped(0.9)))
    .control("gain", |c| c.set_gain(Amplitude(0.3)))
    .check();
}

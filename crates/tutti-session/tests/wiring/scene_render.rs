//! One scene, rendered through the session and pinned to independent
//! reference renders.
//!
//! The graph half of `bevy-tutti`'s `scene_render` suite, which built the
//! scene through the ECS (`spawn_audio_node`, `PortSources`, `MasterSources`,
//! `AudioParam`, `crossfade_audio_node`). The adapter declared exactly the
//! sources written here (each node's input 0 from the previous node, output
//! channels 0 and 1 from the named nodes) and nothing else is spliced into
//! the wiring, so this is the same graph: the same nodes added in the same
//! order, a detached session two outputs wide, the rate set to 48 kHz before
//! the audio side is taken, and the audio side played in device-sized blocks.
//!
//! # The oracles
//!
//! Each comparison uses an oracle that shares no code with the session:
//!
//! - **PDC**: the compensated scene's dry left channel is the uncompensated
//!   scene's, delayed by the limiter's lookahead to the frame; its latent
//!   right channel is the same units hand-wired with `GraphBuilder`.
//! - **Block partition**: per-sample units render the same at any block
//!   length, so a scene in 100-frame blocks is the scene in 64-frame blocks.
//! - **A param write** lands on the first frame of the next block: from
//!   there the scene is the one built with the new value (the waveshaper
//!   holds no state).
//! - **A crossfade** starts on the next block's first frame, follows the
//!   law, and ends on the new filter as if it had run from the fade's start:
//!   a hand-wired graph whose filter hears silence until then.

use tutti_graph::NodeKey;
use tutti_graph::{
    CrossfadeCurve, Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status, Unforkable,
};
use tutti_nodes::testing::Osc;
use tutti_nodes::{DistortionNode, LimiterNode, ShapeKind, SvfFilterNode, SvfType};
use tutti_session::{AudioSide, InPort, OutPort, Root, Session};
use tutti_types::MaxBlock;
use tutti_types::{ChannelLayout, Db, Drive, Hz, Q, SampleRate, Samples, Seconds, UnitParam};

const RATE: f64 = 48_000.0;

/// The scene's units, built once for every side.
fn saw() -> ForkByClone<Osc> {
    ForkByClone(Osc::saw(Hz(110.0)))
}
fn low_pass(cutoff: f32) -> SvfFilterNode<f64> {
    SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(cutoff), Q(0.9))
}
fn shaper(drive: f32) -> DistortionNode {
    DistortionNode::new(ShapeKind::Tanh, Drive::new(drive))
}
fn limiter() -> LimiterNode {
    LimiterNode::with_channels(ChannelLayout::MONO, Db(-12.0), Db(-1.0))
}

/// The scene's waveshaper drive, unless a test says otherwise.
const DRIVE: f32 = 1.5;

/// The nodes a scene declares, for the tests that edit it.
struct Scene {
    session: Session,
    side: AudioSide,
    filter: NodeKey,
    drive: NodeKey,
}

/// A saw through a low-pass and a waveshaper on the left channel, the same
/// saw through a lookahead limiter on the right: so PDC has one latent path
/// to align against a dry one. Without the limiter, the right channel is
/// the saw.
fn scene(with_limiter: bool) -> Scene {
    scene_driven(with_limiter, DRIVE)
}

/// [`scene`], with the waveshaper at `drive`.
fn scene_driven(with_limiter: bool, drive: f32) -> Scene {
    let mut session = Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate::DEFAULT)
        .expect("builds");
    session.graph().set_sample_rate(SampleRate(RATE));
    let side = session.graph().take_audio_side();

    let g = session.graph();
    let (osc, _) = g.add(saw());
    let (filter, _) = g.add(low_pass(1_200.0));
    g.set_sources_hard(InPort::new(filter, 0), [OutPort::new(osc, 0)])
        .unwrap();
    let (drive, _) = g.add(shaper(drive));
    g.set_sources_hard(InPort::new(drive, 0), [OutPort::new(filter, 0)])
        .unwrap();
    let right = if with_limiter {
        let (lim, _) = g.add(limiter());
        g.set_sources_hard(InPort::new(lim, 0), [OutPort::new(osc, 0)])
            .unwrap();
        lim.erase()
    } else {
        osc.erase()
    };
    g.set_sources_hard(Root::output(0), [OutPort::new(drive, 0)])
        .unwrap();
    g.set_sources_hard(Root::output(1), [OutPort::new(right, 0)])
        .unwrap();
    let _ = session.update();
    Scene {
        session,
        side,
        filter: filter.erase(),
        drive: drive.erase(),
    }
}

/// `frames` stereo frames from `side`, in `block`-frame device blocks.
fn render(side: &mut AudioSide, frames: usize, block: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); 2];
    side.render(frames, block, &mut out);
    out
}

/// `frames` of a hand-wired graph (`GraphBuilder`), in 256-frame blocks.
fn render_builder(g: GraphBuilder, frames: usize) -> Vec<Vec<f32>> {
    let mut r = g
        .renderer(Prepare::new(SampleRate(RATE), MaxBlock::of(Samples(256))))
        .expect("builds");
    r.render(frames)
}

/// The first frame where two renders differ, and both values, or `None`.
fn first_difference(a: &[Vec<f32>], b: &[Vec<f32>]) -> Option<(usize, usize, f32, f32)> {
    for (c, (x, y)) in a.iter().zip(b).enumerate() {
        assert_eq!(x.len(), y.len());
        if let Some(f) = (0..x.len()).find(|&f| x[f].to_bits() != y[f].to_bits()) {
            return Some((c, f, x[f], y[f]));
        }
    }
    None
}

/// Not vacuous: a render the comparisons below would pass on if both were
/// silent is refused.
fn assert_sounds(out: &[Vec<f32>], what: &str) {
    for (c, ch) in out.iter().enumerate() {
        let peak = ch.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.05, "{what}: channel {c} is silent (peak {peak})");
    }
}

/// **The compensated scene**: its dry left channel is the uncompensated
/// scene's, delayed by the limiter's lookahead to the frame (silence
/// first); its latent right channel is the saw through the limiter,
/// hand-wired.
#[test]
fn a_compensated_scene_delays_its_dry_channel_by_the_lookahead() {
    let mut s = scene(true);
    let lookahead = s.session.graph_ref().latency_plan().total().get();
    // The limiter's default lookahead, in frames at `RATE`.
    assert!(lookahead > 0, "the limiter reports its lookahead");
    let b = render(&mut s.side, 9_600, 256);
    assert_sounds(&b, "compensated");

    let dry = render(&mut scene(false).side, 9_600, 256);
    let mut delayed = vec![0.0f32; lookahead];
    delayed.extend_from_slice(&dry[0][..9_600 - lookahead]);
    assert_eq!(
        first_difference(&[delayed], &b[..1]),
        None,
        "the left channel is the dry scene's, {lookahead} frames late"
    );

    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let osc = g.add(saw());
    let lim = g.add_with_controls(limiter()).0;
    g.connect(osc, 0, lim, 0).connect_output(lim, 0, 0);
    let latent = render_builder(g, 9_600);
    assert_eq!(
        first_difference(&latent, &b[1..]),
        None,
        "the right channel is the saw through the limiter"
    );
}

/// **A block length that is not a multiple of 64 changes nothing for
/// per-sample nodes**: an oscillator, a filter and a waveshaper render the
/// same samples in 100-frame blocks as in 64-frame blocks, to the bit.
#[test]
fn unaligned_blocks_render_per_sample_units_identically() {
    // 4 850 = 48 × 100 + 50; the reference covers it in whole 64-frame
    // blocks (76 × 64 = 4 864), so it has no partial block of its own.
    let b = render(&mut scene(false).side, 4_850, 100);
    let a: Vec<Vec<f32>> = render(&mut scene(false).side, 4_864, 64)
        .into_iter()
        .map(|mut c| {
            c.truncate(4_850);
            c
        })
        .collect();
    assert_sounds(&b, "100-frame blocks");
    assert_eq!(first_difference(&a, &b), None, "64 vs 100-frame blocks");
}

/// **A param write lands on the first frame of the next block**: written
/// between two 256-frame blocks, the render from there on is the scene built
/// with the new drive (the waveshaper holds no state), and before it the
/// scene with the old one.
#[test]
fn a_param_write_lands_on_the_next_blocks_first_frame() {
    let mut s = scene(false);
    let before = render(&mut s.side, 2_048, 256);
    s.session
        .graph()
        .set_param(s.drive, UnitParam::Drive, Drive(6.0).0);
    let _ = s.session.update();
    let after = render(&mut s.side, 2_048, 256);

    let mut old = scene(false);
    let old_before = render(&mut old.side, 2_048, 256);
    let old_after = render(&mut old.side, 2_048, 256);
    let mut new = scene_driven(false, 6.0);
    render(&mut new.side, 2_048, 256);
    let new_after = render(&mut new.side, 2_048, 256);

    assert_eq!(first_difference(&before, &old_before), None, "before");
    assert_eq!(
        first_difference(&after, &new_after),
        None,
        "after the write: the scene at the new drive"
    );
    // Not vacuous: the write moved the left channel from its first frame.
    assert!(
        matches!(
            first_difference(&after[..1], &old_after[..1]),
            Some((0, 0, _, _))
        ),
        "the write must change the left channel from the first frame after it"
    );
}

/// Passes its input through from frame `from` on, silence before it: the
/// new filter of [`a_crossfade_follows_its_law_to_the_new_filter`]'s oracle
/// hears nothing until the fade starts, which is a filter fresh there.
struct From {
    from: u64,
}

impl Node for From {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for k in cx.env.offsets() {
            let x = io.input(0)[k.index()];
            io.output(0)[k.index()] = if cx.env.frame_at(k).get() >= self.from {
                x
            } else {
                0.0
            };
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A crossfade follows its law to the new filter.** `Graph::crossfade`
/// fades the filter to one with another cutoff over 5 ms (240 frames),
/// starting on the first frame of the next block (1 024), along
/// `CrossfadeCurve::EqualAmplitude`:
///
/// - before it the scene is the unfaded one, to the bit;
/// - from its end on it is the new filter as if it had run from the fade's
///   start (a hand-wired graph whose filter hears silence until then: a
///   state-variable filter fed zeros stays at rest), to the bit;
/// - inside it, frame `k` of the fade is the waveshaper (`tanh(drive · x)`)
///   of the two filters' outputs (hand-wired, before the shaper) blended by
///   the law written out here: `x = (k + 1) / 241`, `g_in = x³(6x² − 15x +
///   10)`, `g_out = 1 − g_in`. Within 1e-6: the blend and `tanh` are
///   computed in `f64` here, in `f32` by the graph.
#[test]
fn a_crossfade_follows_its_law_to_the_new_filter() {
    const FADE_AT: usize = 1_024;
    const FADE: usize = 240;
    let mut s = scene(false);
    let mut b = render(&mut s.side, FADE_AT, 256);
    s.session
        .graph()
        .crossfade(
            s.filter,
            low_pass(300.0),
            Seconds(0.005),
            CrossfadeCurve::EqualAmplitude,
        )
        .expect("the crossfade starts");
    let _ = s.session.update();
    for (c, rest) in b.iter_mut().zip(render(&mut s.side, 3_072 - FADE_AT, 256)) {
        c.extend(rest);
    }

    let old = render(&mut scene(false).side, 3_072, 256);
    // The shaper's input on both sides of the fade, and its output after it:
    // the old filter, the new one heard from the fade's start, and the new
    // one shaped.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from_count(3));
    let osc = g.add(saw());
    let old_filter = g.add_with_controls(low_pass(1_200.0)).0;
    let gate = g.add(Unforkable(From {
        from: FADE_AT as u64,
    }));
    let filter = g.add_with_controls(low_pass(300.0)).0;
    let drive = g.add_with_controls(shaper(DRIVE)).0;
    g.connect(osc, 0, old_filter, 0)
        .connect(osc, 0, gate, 0)
        .connect(gate, 0, filter, 0)
        .connect(filter, 0, drive, 0)
        .connect_output(old_filter, 0, 0)
        .connect_output(filter, 0, 1)
        .connect_output(drive, 0, 2);
    let taps = render_builder(g, 3_072);

    let (b, old) = (&b[0], &old[0]);
    let (old_in, new_in, new) = (&taps[0], &taps[1], &taps[2]);
    assert_eq!(
        first_difference(&[old[..FADE_AT].to_vec()], &[b[..FADE_AT].to_vec()]),
        None,
        "before the fade"
    );
    assert_eq!(
        first_difference(
            &[new[FADE_AT + FADE..].to_vec()],
            &[b[FADE_AT + FADE..].to_vec()]
        ),
        None,
        "after the fade: the new filter, run from the fade's start"
    );
    for k in 0..FADE {
        let f = FADE_AT + k;
        let x = (k + 1) as f64 / (FADE + 1) as f64;
        let g_in = x * x * x * (6.0 * x * x - 15.0 * x + 10.0);
        let blend = (1.0 - g_in) * f64::from(old_in[f]) + g_in * f64::from(new_in[f]);
        let want = (f64::from(DRIVE) * blend).tanh();
        assert!(
            (f64::from(b[f]) - want).abs() < 1e-6,
            "fade frame {k}: {}, the law gives {want}",
            b[f]
        );
    }
    // Not vacuous: the two filters part inside the fade, so the law is heard.
    assert!(
        (FADE_AT..FADE_AT + FADE).any(|f| (old_in[f] - new_in[f]).abs() > 1e-2),
        "the filters differ inside the fade"
    );
}

/// **PDC is the compiler's: nothing is spliced into the wiring.** The
/// compensated scene's dry left channel still reads the waveshaper itself,
/// and the plan carries the delay (it renders the delayed dry channel:
/// [`a_compensated_scene_delays_its_dry_channel_by_the_lookahead`]).
///
/// This is what tells a compiled compensation from a spliced one.
#[test]
fn pdc_is_the_compilers() {
    let mut s = scene(true);
    assert_eq!(
        s.session.graph().sources(Root::output(0)),
        [OutPort::new(s.drive, 0)],
        "nothing is spliced in; the plan delays the channel"
    );
    assert!(
        s.session.graph_ref().latency_plan().total().get() > 0,
        "the limiter reports its lookahead"
    );
}

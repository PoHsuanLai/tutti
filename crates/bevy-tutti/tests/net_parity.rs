//! The same scene, rendered through the adapter and through the `Net`-era
//! graph it replaced (design doc 013, Phase 3): the adapter's graph, driven
//! by the reconcile pipeline, against fundsp's `Net` wired as the adapter
//! wired it on `GraphBackend::Net` before PR 13 deleted that arm.
//!
//! The adapter side of every scene is built through the ECS —
//! `spawn_audio_node`, `PortSources`, `MasterSources`, `AudioParam`,
//! `crossfade_audio_node`, `LatencyCompensationPlugin` — so what is compared
//! is the whole adapter, not a hand-wired graph. It takes the audio side
//! (`AudioGraphRes::take_audio_side`) and plays it in device-sized blocks.
//!
//! **The `Net` side is a test oracle, and nothing else** ([`NetEra`]): the
//! same units in a fundsp `Net`, wired, compensated and committed in the
//! order the adapter's `Net` arm did, and rendered as its audio side
//! rendered (64-frame `process` calls, as `tutti_core::Engine` rendered a
//! `Net`). These were A/B tests between the two arms from PR 11 to PR 13;
//! with one arm gone, the oracle keeps every assertion they made. It goes
//! when `Net` does (doc 013, PR 15 and Phase 5).
//!
//! # What must match, and to what precision
//!
//! - **Bit for bit** wherever doc 013's rules allow: per-sample units (an
//!   oscillator, a state-variable filter, a waveshaper), a latent node under
//!   PDC (`Net` spliced a `PdcDelay`, the compiler a delay ring; both delay
//!   by the same whole frames), and a param write, which lands at the start
//!   of the next block on both when blocks are multiples of 64 (`Net` drained
//!   its queue at the start of each 64-frame `process` call, the node's ring
//!   drains at the start of its block).
//! - **Unaligned blocks.** A per-sample unit's output does not depend on how
//!   its block is chunked, so a 100-frame block still matches bit for bit.
//!   (`Legacy` chunks each block at 64 from its own start, `Net` every 64
//!   frames of device time; a block-oriented unit — an FFT, a batcher —
//!   could differ by where the chunks fall, which is why the scenes here are
//!   per-sample. Doc 013, "Two things carry over from `Legacy` chunking".)
//! - **A crossfade** follows one law on both (`CrossfadeCurve::gains` is
//!   fundsp's `smooth5`, pinned in `tutti-core`'s `crossfade.rs`) but each
//!   runtime places its frames on its own grid, so inside the fade the two
//!   agree to a bound, not to the bit; before it and after it they are equal.

#[macro_use]
mod common;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_tutti::graph::latency::LatencyCompensationPlugin;
use bevy_tutti::graph::{
    crossfade_audio_node, AudioGraphRes, AudioParam, AudioParamAppExt, AudioSide,
    GraphReconcilePlugin, GraphSource, MasterSources, PortSource, PortSources, SpawnAudioNode,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{Net, NodeId, Source};
use tutti_core::{
    AudioUnit, ChannelLayout, CrossfadeCurve, Db, Drive, Hz, NetBackend, SampleRate, UnitParam, Q,
};
use tutti_nodes::testing::Osc;
use tutti_nodes::{DistortionNode, LimiterNode, ShapeKind, SvfFilterNode, SvfType};

const RATE: f64 = 48_000.0;

type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

/// The scene's units, built once for either side.
fn saw() -> Osc {
    Osc::saw(Hz(110.0))
}
fn low_pass(cutoff: f32) -> SvfFilterNode<f64> {
    SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(cutoff), Q(0.9))
}
fn shaper() -> DistortionNode {
    DistortionNode::new(ShapeKind::Tanh, 1.5)
}
fn limiter() -> LimiterNode {
    LimiterNode::with_channels(ChannelLayout::MONO, Db(-12.0), Db(-1.0))
}

/// The entities a scene declares, for the tests that edit it.
struct Scene {
    app: App,
    side: AudioSide,
    filter: Entity,
    drive: Entity,
}

/// A saw through a low-pass and a waveshaper on the left channel, the same
/// saw through a lookahead limiter on the right — so PDC has one latent path
/// to align against a dry one.
fn scene(with_limiter: bool) -> Scene {
    let mut graph = AudioGraphRes::headless(0, 2);
    graph.set_sample_rate(SampleRate(RATE));
    let side = graph.take_audio_side();

    let mut app = App::new();
    app.insert_resource(graph);
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, LatencyCompensationPlugin));
    // Under `modulation` the param reconciler asks the matrix whether a param
    // has a second writer, so the plugin that owns it must be present.
    #[cfg(feature = "modulation")]
    {
        app.insert_resource(bevy_tutti::graph::TransportRes(
            tutti_core::transport::Transport::new(RATE),
        ));
        app.add_plugins(bevy_tutti::modulation::TuttiModulationPlugin);
    }
    app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

    let world = app.world_mut();
    let mut commands = world.commands();
    let osc = commands.spawn_audio_node(saw()).id();
    let filter = commands
        .spawn_audio_node(low_pass(1_200.0))
        .insert(PortSources::from(osc))
        .id();
    let drive = commands
        .spawn_audio_node(shaper())
        .insert(PortSources::from(filter))
        .id();
    let right = if with_limiter {
        commands
            .spawn_audio_node(limiter())
            .insert(PortSources::from(osc))
            .id()
    } else {
        osc
    };
    commands.insert_resource(
        MasterSources::default()
            .with(0, PortSource::node(drive))
            .with(1, PortSource::node(right)),
    );
    world.flush();
    app.update();
    Scene {
        app,
        side,
        filter,
        drive,
    }
}

/// [`scene`] as the adapter built it on `GraphBackend::Net`: a `Net` at
/// `RATE` whose backend was taken before any node went in
/// (`take_audio_side`), the units pushed in the order the ECS spawned them,
/// wired as the declarations say, compensated (`latency::compensate`, which
/// splices `PdcDelay` nodes) and committed, as the first frame's
/// `Compensate` and `Commit` did.
struct NetEra {
    net: Net,
    backend: NetBackend,
    filter: NodeId,
    drive: NodeId,
}

impl NetEra {
    fn scene(with_limiter: bool) -> Self {
        let mut net = Net::new(0, 2);
        net.set_sample_rate(SampleRate(RATE));
        let backend = net.backend();
        let osc = net.push(Box::new(saw()));
        let filter = net.push(Box::new(low_pass(1_200.0)));
        let drive = net.push(Box::new(shaper()));
        let right = if with_limiter {
            let lim = net.push(Box::new(limiter()));
            net.set_source(lim, 0, Source::Local(osc, 0));
            lim
        } else {
            osc
        };
        net.set_source(filter, 0, Source::Local(osc, 0));
        net.set_source(drive, 0, Source::Local(filter, 0));
        net.set_output_source(0, Source::Local(drive, 0));
        net.set_output_source(1, Source::Local(right, 0));
        let mut era = Self {
            net,
            backend,
            filter,
            drive,
        };
        era.commit_frame();
        era
    }

    /// A frame that edited the graph: `compensate_graph` then `commit_graph`
    /// on the `Net` arm.
    fn commit_frame(&mut self) {
        tutti_core::latency::compensate(&mut self.net);
        self.net.commit_output_arity_change();
    }

    /// `AudioGraphRes::set_param` on the `Net` arm: a setting addressed to the
    /// node, queued for the backend (no commit).
    fn set_drive(&mut self, value: f32) {
        self.net.set(tutti_core::unit_param::node_setting(
            self.drive,
            UnitParam::Drive,
            value,
        ));
    }

    /// `crossfade_audio_node` on the `Net` arm: `Net::crossfade` with the
    /// adapter's 5 ms equal-amplitude fade, then the frame's commit.
    fn crossfade_filter(&mut self, unit: Box<dyn AudioUnit>) {
        self.net.crossfade(
            self.filter,
            tutti_core::net_fade(CrossfadeCurve::EqualAmplitude),
            0.005,
            unit,
        );
        self.commit_frame();
    }

    /// `frames` stereo frames, as the `Net` arm's `AudioSide::render` played
    /// them: `process` in chunks of at most `MAX_BUFFER_SIZE` inside each
    /// `block`, as `tutti_core::Engine` rendered a `Net`.
    fn render(&mut self, frames: usize, block: usize) -> Vec<Vec<f32>> {
        let width = self.backend.outputs();
        let mut buf = tutti_core::BufferVec::new(width);
        let none = tutti_core::BufferVec::new(self.backend.inputs());
        let mut out = vec![vec![0.0f32; frames]; 2];
        let mut done = 0;
        while done < frames {
            let len = (frames - done).min(block).min(tutti_core::MAX_BUFFER_SIZE);
            self.backend
                .process(len, &none.buffer_ref(), &mut buf.buffer_mut());
            for (c, o) in out.iter_mut().enumerate().take(width) {
                o[done..done + len].copy_from_slice(&buf.channel_f32(c)[..len]);
            }
            done += len;
        }
        out
    }
}

/// `frames` stereo frames from `side`, in `block`-frame device blocks.
fn render(side: &mut AudioSide, frames: usize, block: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); 2];
    side.render(frames, block, &mut out);
    out
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

/// **The scene renders the `Net` era's samples bit for bit**, the latent
/// right channel included: the limiter's lookahead is compensated on the dry
/// left channel by the compiler, as `Net` compensated it with a `PdcDelay`,
/// to the same frame.
///
/// Mutations (run; each fails this test):
/// - the forwarding `Boxed::latency` returning `None` → the editor never
///   learns the lookahead, and the left channel leads by it;
/// - `AudioSide::render` dropping a trailing partial block (9 600 frames is
///   37½ blocks) → its tail is silent.
#[test]
fn a_scene_renders_the_net_eras_samples() {
    let mut net = NetEra::scene(true);
    let mut native = scene(true);
    // Blocks of 256: every `Legacy` chunk lands where `Net`'s does.
    let a = net.render(9_600, 256);
    let b = render(&mut native.side, 9_600, 256);
    assert_sounds(&b, "native");
    assert_eq!(first_difference(&a, &b), None, "net era vs native");
}

/// **A block length that is not a multiple of 64 changes nothing for
/// per-sample units**: `Legacy` chunks each 100-frame block from its own
/// start, `Net` every 64 frames of device time, and an oscillator, a filter
/// and a waveshaper render the same samples either way.
///
/// Mutation (run): `AudioSide::render` rendering whole blocks only (`while
/// done + block <= frames`) leaves the last 50 frames at zero and fails.
#[test]
fn unaligned_blocks_render_per_sample_units_identically() {
    let mut net = NetEra::scene(false);
    let mut native = scene(false);
    let a = net.render(4_850, 100);
    let b = render(&mut native.side, 4_850, 100);
    assert_sounds(&b, "native");
    assert_eq!(first_difference(&a, &b), None, "net era vs native");
}

/// **A param write lands on the frame the `Net` era landed it on.** `Net`
/// queued it for its backend, which applied it at the start of its next
/// 64-frame `process` call; the node's settings ring is drained at the start
/// of its next block. Written between two 256-frame blocks, both land it on
/// the first frame of the next one.
///
/// Mutation (run): `NativeGraph::set_param` sending the setting with the node
/// address `Net` needed (`unit_param::node_setting`) instead of the leaf's →
/// the waveshaper ignores it and the renders part at the write.
#[test]
fn a_param_write_lands_on_the_net_eras_frame() {
    let mut net = NetEra::scene(false);
    let mut native = scene(false);

    let a_before = net.render(2_048, 256);
    net.set_drive(6.0);
    let a_after = net.render(2_048, 256);

    let b_before = render(&mut native.side, 2_048, 256);
    native
        .app
        .world_mut()
        .entity_mut(native.drive)
        .insert(DriveParam::new(Drive(6.0)));
    native.app.update();
    let b_after = render(&mut native.side, 2_048, 256);

    // Not vacuous: against the same scene with no write, the left channel
    // parts on the write's block, at its first frame.
    let mut control = scene(false);
    render(&mut control.side, 2_048, 256);
    let unwritten = render(&mut control.side, 2_048, 256);
    let parted = first_difference(&b_after[..1], &unwritten[..1]);
    assert!(
        matches!(parted, Some((0, 0, _, _))),
        "the write must change the left channel from the first frame after it: {parted:?}"
    );
    assert_eq!(
        first_difference(&a_before, &b_before),
        None,
        "before the write"
    );
    assert_eq!(
        first_difference(&a_after, &b_after),
        None,
        "after the write"
    );
}

/// **A crossfade ends on the `Net` era's samples.** `crossfade_audio_node`
/// fades the filter to one with another cutoff over 5 ms, along the law
/// `Net::crossfade` used (`smooth5`). Before the fade and after it the
/// renders are bit-identical; inside it they may differ by where each
/// runtime places its fade frames, which moves a gain by at most one fade
/// step — bounded here by the step of a 240-frame fade (5 ms at 48 kHz)
/// times the largest sample.
///
/// Mutation (run): `NativeGraph::replace` landing the unit with
/// `Editor::insert` even when it fits (a hard swap) → the fade region jumps
/// by the full difference between the two filters, far past the bound.
#[test]
fn a_crossfade_ends_on_the_net_eras_samples() {
    let mut net = NetEra::scene(false);
    let a = {
        let before = net.render(1_024, 256);
        net.crossfade_filter(Box::new(low_pass(300.0)));
        let fade = net.render(512, 256);
        let after = net.render(2_048, 256);
        (before, fade, after)
    };
    let mut s = scene(false);
    let b = {
        let before = render(&mut s.side, 1_024, 256);
        crossfade_audio_node(
            &mut s.app.world_mut().commands(),
            s.filter,
            Box::new(low_pass(300.0)),
        );
        s.app.world_mut().flush();
        s.app.update();
        let fade = render(&mut s.side, 512, 256);
        let after = render(&mut s.side, 2_048, 256);
        (before, fade, after)
    };
    assert_eq!(first_difference(&a.0, &b.0), None, "before the fade");
    assert_eq!(first_difference(&a.2, &b.2), None, "after the fade");
    // One fade step of a 240-frame fade, at the largest sample either side
    // produced. The two filters differ by far more than this inside the fade
    // (a hard swap misses it by orders of magnitude).
    let step = 1.0 / 241.0;
    let peak = a.1[0]
        .iter()
        .chain(&b.1[0])
        .fold(0.0f32, |m, x| m.max(x.abs()));
    let worst = a.1[0]
        .iter()
        .zip(&b.1[0])
        .fold(0.0f32, |m, (x, y)| m.max((x - y).abs()));
    assert!(
        worst <= 2.0 * step * peak + 1e-6,
        "inside the fade the runtimes differ by {worst}, past two fade steps ({})",
        2.0 * step * peak
    );
}

/// **PDC is the compiler's: nothing is spliced into the wiring.** The
/// compensated scene's dry left channel still reads the waveshaper itself,
/// and the plan carries the delay (it renders the `Net` era's compensated
/// samples: [`a_scene_renders_the_net_eras_samples`]).
///
/// Until PR 13 this also pinned the other half, on the adapter's `Net` arm:
/// there the channel read a `PdcDelay` the adapter had spliced in. That arm
/// is gone; the half that stays is what tells a compiled compensation from a
/// spliced one.
///
/// Mutation (run): `NativeGraph::lift` answering `Silence` for a node source
/// → the channel no longer reads the waveshaper; the rebuild's own
/// consistency check (`topology::disagreements`) panics on it first.
#[test]
fn pdc_is_the_compilers() {
    let s = scene(true);
    let drive = *s
        .app
        .world()
        .get::<tutti_core::AudioNode>(s.drive)
        .expect("bound");
    let graph = s.app.world().resource::<AudioGraphRes>();
    assert_eq!(
        graph.output_source(0),
        GraphSource::Node(drive, 0),
        "nothing is spliced in; the plan delays the channel"
    );
    assert!(
        graph.latency_plan().total().get() > 0,
        "the limiter reports its lookahead"
    );
}

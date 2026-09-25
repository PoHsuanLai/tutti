//! The same scene, rendered through both graph backends (design doc 013,
//! Phase 3 PR 11): `Net` and the native `tutti-graph` runtime behind
//! `AudioGraphRes`, driven by the same reconcile pipeline.
//!
//! Every scene here is built through the ECS — `spawn_audio_node`,
//! `PortSources`, `MasterSources`, `AudioParam`, `crossfade_audio_node`,
//! `LatencyCompensationPlugin` — so what is compared is the whole adapter on
//! each backend, not two hand-wired graphs. Each render takes the audio side
//! (`AudioGraphRes::take_audio_side`) and plays it in device-sized blocks.
//!
//! # What must match, and to what precision
//!
//! - **Bit for bit** wherever doc 013's rules allow: per-sample units (an
//!   oscillator, a state-variable filter, a waveshaper), a latent node under
//!   PDC (the `Net` backend splices a `PdcDelay`, the native compiler a delay
//!   ring; both delay by the same whole frames), and a param write, which
//!   lands at the start of the next block on both when blocks are multiples
//!   of 64 (`Net` drains its queue at the start of each 64-frame `process`
//!   call, the native node's ring at the start of its block).
//! - **Unaligned blocks.** A per-sample unit's output does not depend on how
//!   its block is chunked, so a 100-frame block still matches bit for bit.
//!   (`Legacy` chunks each native block at 64 from its own start, `Net` every
//!   64 frames of device time; a block-oriented unit — an FFT, a batcher —
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
    crossfade_audio_node, AudioGraphRes, AudioParam, AudioParamAppExt, AudioSide, GraphBackend,
    GraphReconcilePlugin, GraphSource, MasterSources, PortSource, PortSources, SpawnAudioNode,
};
use bevy_tutti::AudioEngineState;
use tutti_core::{ChannelLayout, Db, Drive, Hz, SampleRate, UnitParam, Q};
use tutti_nodes::testing::Osc;
use tutti_nodes::{DistortionNode, LimiterNode, ShapeKind, SvfFilterNode, SvfType};

const RATE: f64 = 48_000.0;

type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

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
fn scene(backend: GraphBackend, with_limiter: bool) -> Scene {
    let mut graph = AudioGraphRes::unattached_with(backend, 0, 2);
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
    let osc = commands.spawn_audio_node(Osc::saw(Hz(110.0))).id();
    let filter = commands
        .spawn_audio_node(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz(1_200.0),
            Q(0.9),
        ))
        .insert(PortSources::from(osc))
        .id();
    let drive = commands
        .spawn_audio_node(DistortionNode::new(ShapeKind::Tanh, 1.5))
        .insert(PortSources::from(filter))
        .id();
    let right = if with_limiter {
        commands
            .spawn_audio_node(LimiterNode::with_channels(
                ChannelLayout::MONO,
                Db(-12.0),
                Db(-1.0),
            ))
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

/// **The scene renders bit-identically on both backends**, the latent right
/// channel included: the limiter's lookahead is compensated on the dry left
/// channel by a `PdcDelay` on `Net` and by the compiler on `Native`, to the
/// same frame.
///
/// Mutations (run; each fails this test):
/// - `Net`'s `compensate` planning without splicing (`latency::plan` for
///   `latency::compensate`) → the left channel leads by the lookahead on
///   `Net`;
/// - the forwarding `Boxed::latency` returning `None` → the native editor
///   never learns the lookahead, and its left channel leads instead;
/// - `AudioSide::render` dropping a trailing partial block on `Native`
///   (9 600 frames is 37½ blocks) → its tail is silent.
#[test]
fn a_scene_renders_bit_identically_on_both_backends() {
    let mut net = scene(GraphBackend::Net, true);
    let mut native = scene(GraphBackend::Native, true);
    // Blocks of 256: every `Legacy` chunk lands where `Net`'s does.
    let a = render(&mut net.side, 9_600, 256);
    let b = render(&mut native.side, 9_600, 256);
    assert_sounds(&a, "net");
    assert_eq!(first_difference(&a, &b), None, "net vs native");
}

/// **A block length that is not a multiple of 64 changes nothing for
/// per-sample units**: `Legacy` chunks each 100-frame native block from its
/// own start, `Net` every 64 frames of device time, and an oscillator, a
/// filter and a waveshaper render the same samples either way.
///
/// Mutation (run): `AudioSide::render` rendering whole native blocks only
/// (`while done + block <= frames`) leaves the last 50 frames at zero and
/// fails.
#[test]
fn unaligned_blocks_render_per_sample_units_identically() {
    let mut net = scene(GraphBackend::Net, false);
    let mut native = scene(GraphBackend::Native, false);
    let a = render(&mut net.side, 4_850, 100);
    let b = render(&mut native.side, 4_850, 100);
    assert_sounds(&a, "net");
    assert_eq!(first_difference(&a, &b), None, "net vs native");
}

/// **A param write lands on the same frame on both backends.** `Net` queues
/// it for its backend, which applies it at the start of its next 64-frame
/// `process` call; the native node's settings ring is drained at the start of
/// its next block. Written between two 256-frame blocks, both land it on the
/// first frame of the next one.
///
/// Mutation (run): `NativeGraph::set_param` sending the setting with the node
/// address `Net` needs (`unit_param::node_setting`) instead of the leaf's →
/// the waveshaper ignores it and the renders part at the write.
#[test]
fn a_param_write_lands_on_the_same_frame_on_both_backends() {
    let mut net = scene(GraphBackend::Net, false);
    let mut native = scene(GraphBackend::Native, false);
    let mut runs = [(&mut net, Vec::new()), (&mut native, Vec::new())];
    for (s, out) in &mut runs {
        let before = render(&mut s.side, 2_048, 256);
        s.app
            .world_mut()
            .entity_mut(s.drive)
            .insert(DriveParam::new(Drive(6.0)));
        s.app.update();
        let after = render(&mut s.side, 2_048, 256);
        *out = vec![before, after];
    }
    let [(_, a), (_, b)] = runs;
    // Not vacuous: against the same scene with no write, the left channel
    // parts on the write's block, at its first frame.
    let mut control = scene(GraphBackend::Net, false);
    render(&mut control.side, 2_048, 256);
    let unwritten = render(&mut control.side, 2_048, 256);
    let parted = first_difference(&a[1][..1], &unwritten[..1]);
    assert!(
        matches!(parted, Some((0, 0, _, _))),
        "the write must change the left channel from the first frame after it: {parted:?}"
    );
    for (i, what) in ["before the write", "after the write"].iter().enumerate() {
        assert_eq!(first_difference(&a[i], &b[i]), None, "{what}");
    }
}

/// **A crossfade ends on the same samples.** `crossfade_audio_node` fades the
/// filter to one with another cutoff over 5 ms on both backends, along the
/// same law (`smooth5`). Before the fade and after it the renders are
/// bit-identical; inside it they may differ by where each runtime places its
/// fade frames, which moves a gain by at most one fade step — bounded here by
/// the step of a 240-frame fade (5 ms at 48 kHz) times the largest sample.
///
/// Mutation (run): `NativeGraph::replace` landing the unit with `Editor::insert`
/// even when it fits (a hard swap) → the fade region jumps by the full
/// difference between the two filters, far past the bound.
#[test]
fn a_crossfade_ends_on_the_same_samples() {
    let mut net = scene(GraphBackend::Net, false);
    let mut native = scene(GraphBackend::Native, false);
    let mut outs = Vec::new();
    for s in [&mut net, &mut native] {
        let before = render(&mut s.side, 1_024, 256);
        crossfade_audio_node(
            &mut s.app.world_mut().commands(),
            s.filter,
            Box::new(SvfFilterNode::<f64>::new(
                SvfType::LowPass,
                Hz(300.0),
                Q(0.9),
            )),
        );
        s.app.world_mut().flush();
        s.app.update();
        let fade = render(&mut s.side, 512, 256);
        let after = render(&mut s.side, 2_048, 256);
        outs.push((before, fade, after));
    }
    let (a, b) = (&outs[0], &outs[1]);
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
        "inside the fade the backends differ by {worst}, past two fade steps ({})",
        2.0 * step * peak
    );
}

/// **The switch is real: PDC is a node on `Net` and the compiler's on
/// `Native`.** The same scene, compensated by the same plugin, leaves the dry
/// left channel reading a `PdcDelay` node the adapter spliced in on `Net`, and
/// reading the waveshaper itself on `Native`, whose plan carries the delay.
/// Both render the same samples (`a_scene_renders_bit_identically_…`), so this
/// is what tells the two apart.
///
/// Mutation (run): `AudioGraphRes::with_rate` building a `Net` for
/// `GraphBackend::Native` (a silent fall-back) → the native run finds a delay
/// node on channel 0 and fails; every bit-identity test above still passes,
/// which is why this one exists.
#[test]
fn pdc_is_a_node_on_net_and_the_compilers_on_native() {
    for backend in [GraphBackend::Net, GraphBackend::Native] {
        let s = scene(backend, true);
        let drive = *s
            .app
            .world()
            .get::<tutti_core::AudioNode>(s.drive)
            .expect("bound");
        let graph = s.app.world().resource::<AudioGraphRes>();
        let left = graph.output_source(0);
        match backend {
            GraphBackend::Net => assert!(
                matches!(left, GraphSource::Node(n, 0) if n != drive),
                "Net: the dry channel reads a compensation delay, got {left:?}"
            ),
            GraphBackend::Native => assert_eq!(
                left,
                GraphSource::Node(drive, 0),
                "Native: nothing is spliced in; the plan delays the channel"
            ),
        }
        assert!(
            graph.latency_plan().total().get() > 0,
            "{backend:?}: the limiter reports its lookahead"
        );
    }
}

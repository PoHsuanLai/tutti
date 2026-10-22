//! Modulation on control lanes, in an `App` (doc 014 §8, §12.5).
//!
//! Every test declares its modulation as components ([`Mods`], `ModSource`,
//! `ModRoute`, `ModRange`, `Automation`), runs the app's frames, and asserts
//! on what the graph renders: the nodes are [`ProbeUnit`]s, which write the
//! value their param port reads to their output, so a render is the
//! delivered value frame by frame. The audio side is taken
//! ([`AudioGraphRes::take_audio_side`]) and rendered between frames, as a
//! device would.
//!
//! The properties that need the plan the graph sent (a depth or range edit
//! sends none; a follower route sends one) are the crate's own
//! `modulation::tests`, which can read it.

#![cfg(feature = "modulation")]

#[macro_use]
mod common;

use std::sync::Arc;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    crossfade_audio_node, AudioGraphRes, AudioParam, AudioParamAppExt, AudioSide,
    GraphReconcilePlugin, MasterSources, PortSource, SpawnAudioNode, TransportRes,
};
use bevy_tutti::modulation::{
    Adsr, Automation, AutomationTake, Follower, Lfo, Macro, ModRange, ModRoute, ModSource, Mods,
    ModulationRefused, NoteEnvelope, ParamRef, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_graph::ForkByClone;
use tutti_nodes::automation::{AutomationEnvelope, AutomationPoint, RecordMode};
use tutti_nodes::ReconcileError;
use tutti_types::{Amplitude, Beat, BeatDuration, Bpm, Depth, Hz, SampleRate, Seconds, UnitParam};

use common::probe::{Held, ProbeUnit, Pulse};

const RATE: f64 = 48_000.0;
const TEMPO: Bpm = Bpm(120.0);
/// A lane's control step (`tutti_nodes::automation::CONTROL_STEP`).
const STEP: usize = 32;
/// A port's connect crossfade (`PARAM_DECLICK`).
const DECLICK: usize = 256;

type CutoffParam = AudioParam<Hz, { UnitParam::Cutoff as u16 }>;

/// An app on a headless graph of `outputs` channels at 48 kHz, its audio
/// side taken, with the graph and modulation plugins.
fn app(outputs: usize) -> (App, AudioSide) {
    let mut graph = AudioGraphRes::headless(0, outputs);
    graph.set_sample_rate(SampleRate(RATE));
    let side = graph.take_audio_side();
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(graph);
    app.insert_resource(TransportRes(Transport::new(RATE)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    (app, side)
}

/// Spawns `node` the ordinary way and returns its entity.
fn spawn<N>(app: &mut App, node: N) -> Entity
where
    N: bevy_tutti::graph::SessionNode,
    N::Controls: Send + Sync + 'static,
{
    let entity = app.world_mut().commands().spawn_audio_node(node).id();
    app.world_mut().flush();
    entity
}

/// Output channel `c` from `entity`'s port 0, for each.
fn master(app: &mut App, entities: &[Entity]) {
    let mut sources = MasterSources::default();
    for (c, &e) in entities.iter().enumerate() {
        sources = sources.with(c, PortSource::node(e));
    }
    app.insert_resource(sources);
}

/// Frames until nothing is left to resolve: nodes committed, then lanes.
fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert_eq!(state.refusal(), None, "the declarations apply");
    assert_eq!(
        state.unresolved_targets().count(),
        0,
        "every target resolved"
    );
}

/// `frames` frames of `channels` outputs, stopped, in `block`-frame blocks.
fn render_n(side: &mut AudioSide, channels: usize, frames: usize, block: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); channels];
    side.set_transport(tutti_graph::Transport::new(false, TEMPO, Beat(0.0), None));
    side.render(frames, block, &mut out);
    out
}

/// [`render_n`] of one output.
fn render(side: &mut AudioSide, frames: usize, block: usize) -> Vec<Vec<f32>> {
    render_n(side, 1, frames, block)
}

/// `500 + 1000 · depth · sin(2π · rate · t)` at frame `f`: a bipolar linear
/// sine route on a `0..=1000` target with knob 500.
fn lfo_value(depth: f64, rate: f64, f: usize) -> f64 {
    500.0 + 1000.0 * depth * (std::f64::consts::TAU * rate * f as f64 / RATE).sin()
}

/// Doc 014 §5.3's bound for a linear sine at `rate` on the 32-frame grid,
/// times the swing, plus `f32` rounding of values near 1000.
fn lfo_tolerance(depth: f64, rate: f64) -> f64 {
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    h * h / 8.0 * 1000.0 * depth + 1e-3
}

/// A follower's analytic level (attack `ta`, release `tr`) of a 0.8 pulse
/// on frames `4800..7200`, one control step late and linear between grid
/// points (doc 014 §6), ducking a `0..=1` param with knob 0.8 at depth −0.6.
fn ducked(f: usize, ta: f64, tr: f64) -> f64 {
    let pole = |tau: f64| (-1.0 / (tau * RATE)).exp();
    let (ca, cr) = (pole(ta), pole(tr));
    let env = |g: usize| -> f64 {
        if g <= 4_800 {
            0.0
        } else if g <= 7_200 {
            0.8 * (1.0 - ca.powi(g as i32 - 4_800))
        } else {
            0.8 * (1.0 - ca.powi(2_400)) * cr.powi(g as i32 - 7_200)
        }
    };
    let g = f / STEP * STEP;
    let x = if g < STEP {
        0.0
    } else {
        let (a, b) = (env(g - STEP), env(g));
        a + (b - a) * (f - g) as f64 / STEP as f64
    };
    0.8 - 0.6 * x
}

// ------------------------------------------------------------ §8.1 ----

/// The §8.1 example's entities.
#[derive(Resource, Clone, Copy)]
struct Scene {
    filter: Entity,
    pad: Entity,
    kick_bus: Entity,
}

/// Doc 014 §8.1, word for word but for the ranges a route needs.
fn setup(mut mods: Mods, scene: Res<Scene>) {
    let cutoff = ParamRef::unit(scene.filter, UnitParam::Cutoff);
    let gain = ParamRef::unit(scene.pad, UnitParam::Volume);
    mods.range(cutoff, 0.0, 1_000.0);
    mods.range(gain, 0.0, 1.0);
    let wobble = mods.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    mods.route(wobble, cutoff, Depth(0.3));
    let kick = mods.follower(scene.kick_bus);
    mods.route(kick, gain, Depth(-0.6)); // sidechain
}

/// **Doc 014 §8.1 in an app.** A sine LFO of one cycle per half beat on a
/// cutoff (knob 500 in `0..=1000`, depth 0.3) renders the analytic sine,
/// bar-locked at 120 BPM (4 Hz), within §5.3's bound past the connect's
/// declick; and the pad (knob 0.8 in `0..=1`) dips with the kick through a
/// follower (`Mods::follower`'s 5 ms / 100 ms) at depth −0.6, analytically.
///
/// Mutations (run), each fails: `ModHostOver::target` handing the knob
/// cell of another param (`p.cell(Volume)`: the cutoff has none and reads
/// 500 flat); `Declared::collect` dropping the ranges (the routes are
/// refused); `ModKeys::feed` (`Declared`) answering `None` (the follower is
/// unresolved and the pad never dips).
#[test]
fn the_8_1_example_renders_its_analytic_values() {
    let (mut app, mut side) = app(2);
    let filter = spawn(&mut app, ProbeUnit::new(500.0));
    let pad = spawn(&mut app, ProbeUnit::of(UnitParam::Volume, 0.8));
    let kick_bus = spawn(
        &mut app,
        ForkByClone(Pulse {
            from: 4_800,
            to: 7_200,
            level: 0.8,
        }),
    );
    master(&mut app, &[filter, pad]);
    app.insert_resource(Scene {
        filter,
        pad,
        kick_bus,
    });
    app.world_mut().run_system_cached(setup).expect("runs");
    settle(&mut app);

    // Rolling from beat 0 at frame 0: each block under its own beat.
    let total = 24_000;
    let block = 128;
    let mut all = [Vec::new(), Vec::new()];
    let mut out = vec![Vec::new(), Vec::new()];
    let mut done = 0;
    while done < total {
        let beat = done as f64 * TEMPO.0 / 60.0 / RATE;
        side.set_transport(tutti_graph::Transport::new(true, TEMPO, Beat(beat), None));
        side.render(block, block, &mut out);
        for (a, o) in all.iter_mut().zip(&out) {
            a.extend_from_slice(o);
        }
        done += block;
    }
    let tol = lfo_tolerance(0.3, 4.0);
    for (f, &v) in all[0].iter().enumerate().skip(DECLICK) {
        let want = lfo_value(0.3, 4.0, f);
        assert!(
            (f64::from(v) - want).abs() < tol,
            "cutoff, frame {f}: {v} vs {want}"
        );
    }
    for (f, &v) in all[1].iter().enumerate() {
        let want = ducked(f, 0.005, 0.1);
        assert!(
            (f64::from(v) - want).abs() < 1e-4,
            "pad, frame {f}: {v} vs {want}"
        );
    }
    let low = all[1].iter().copied().fold(f32::INFINITY, f32::min);
    assert!(low < 0.4, "the pad dips with the kick: {low}");
}

// --------------------------------------------------------- export --

/// **Export == live in an app, on a native param.** A 3 Hz sine on
/// one probe's cutoff and a 5 Hz triangle on another's volume, their knobs
/// moved after the lanes are built (a knob is the node's cell, which a
/// fork reads); live plays from a stop into rolling ragged blocks, the
/// export (`ExportPlugin`, buffers) forks the graph and renders from its
/// first frame. Equal bit for bit from the play start.
///
/// Mutation (run): `ModHostOver::target` handing the lane a copy of the
/// knob cell, not the node's own (the live lane keeps the value it was
/// built with, the export forks the node's: frame 0 reads 623.5 live
/// against 450 exported) → fails.
#[cfg(feature = "export")]
#[test]
fn an_export_is_the_live_render_on_a_native_param() {
    use bevy_tutti::export::{
        ExportClock, ExportDone, ExportOutput, ExportPlugin, ExportRequest, ExportSource,
        ExportTarget,
    };
    use std::sync::Mutex;
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_export::{AudioFormat, BitDepth, EncodeConfig, ExportConfig, RenderConfig};

    let (mut app, mut side) = app(2);
    app.add_plugins(ExportPlugin);
    app.insert_resource(bevy_tutti::graph::AudioConfig {
        sample_rate: SampleRate(RATE),
        channels: tutti_core::ChannelLayout::STEREO,
    });
    app.insert_resource(bevy_tutti::graph::MetronomeRes(Arc::new(
        tutti_core::transport::ClickState::new(),
    )));
    app.add_audio_param::<Hz, { UnitParam::Cutoff as u16 }>();
    let a = spawn(&mut app, ProbeUnit::new(500.0));
    let b = spawn(&mut app, ProbeUnit::of(UnitParam::Volume, 0.5));
    master(&mut app, &[a, b]);
    let world = app.world_mut();
    let cutoff = ParamRef::unit(a, UnitParam::Cutoff);
    let volume = ParamRef::unit(b, UnitParam::Volume);
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    world.spawn(ModRange::new(volume, 0.0, 1.0));
    let sine = world.spawn(ModSource::new(Lfo::sine().hz(Hz(3.0)))).id();
    let tri = world
        .spawn(ModSource::new(Lfo::triangle().hz(Hz(5.0))))
        .id();
    world.spawn(ModRoute::new(sine, cutoff).with_depth(Depth(0.3)));
    world.spawn(ModRoute::new(tri, volume).with_depth(Depth(0.2)));
    settle(&mut app);
    // Past the connect's declick, stopped; then the knobs move.
    render_n(&mut side, 2, 1_024, 128);
    app.world_mut()
        .entity_mut(a)
        .insert(CutoffParam::new(Hz(450.0)));
    app.update();
    // The same write lands on the live side before it plays.
    render_n(&mut side, 2, 128, 128);

    let total = 24_000;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: TEMPO,
        sample_rate: SampleRate(RATE),
        loop_range: None,
    }));
    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(RATE),
            duration_seconds: total as f64 / RATE,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: BitDepth::Float32,
            channels: tutti_core::ChannelLayout::STEREO,
        },
        ..Default::default()
    };
    let slot: Arc<Mutex<Option<Vec<Vec<f32>>>>> = Arc::default();
    let seen = Arc::clone(&slot);
    app.world_mut()
        .spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::Buffers,
            config,
            ExportClock::timeline(timeline),
        ))
        .observe(move |done: On<ExportDone>| match &done.result {
            Ok(ExportOutput::Buffers(r)) => *seen.lock().unwrap() = Some(r.planes.clone()),
            other => panic!("the export failed: {other:?}"),
        });
    app.update();

    // Live: rolling from beat 0 (the play clock restarts there), in
    // ragged blocks.
    let mut live = [Vec::new(), Vec::new()];
    let mut out = vec![Vec::new(), Vec::new()];
    let mut done = 0;
    for n in [1usize, 128, 7, 64, 127, 33, 2, 100].iter().cycle() {
        if done >= total {
            break;
        }
        let n = (*n).min(total - done);
        let beat = done as f64 * TEMPO.0 / 60.0 / RATE;
        side.set_transport(tutti_graph::Transport::new(true, TEMPO, Beat(beat), None));
        side.render(n, n, &mut out);
        for (l, o) in live.iter_mut().zip(&out) {
            l.extend_from_slice(o);
        }
        done += n;
    }

    let mut planes = None;
    for _ in 0..6_000 {
        app.update();
        if let Some(p) = slot.lock().unwrap().take() {
            planes = Some(p);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let export = planes.expect("the export reported");
    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(e.len() >= total, "channel {c}: {} frames", e.len());
        assert!(l.iter().any(|&v| v != l[0]), "channel {c} moves");
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f}: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
    // The knob written after the lanes were built is what both play.
    let want = 450.0 + 300.0 * (std::f64::consts::TAU * 3.0 * 12_000.0 / RATE).sin();
    assert!((f64::from(export[0][12_000]) - want).abs() < lfo_tolerance(0.3, 3.0));
}

// ------------------------------------------ removal and the knob --

/// **A despawned source leaves no node and no layer.** A follower and
/// an LFO on one cutoff; the follower's source entity is despawned (its
/// route stays, naming a source that is gone). Its node leaves the graph,
/// the lane's program reads no input and keeps the LFO's layer, and the
/// reconciler holds nothing for it.
///
/// Mutation (run): `Declared::collect` keeping a route whose source is gone
/// (`self.sources.contains(r.source)` dropped) fails: the compile refuses
/// an unknown source, so nothing changes and the node stays.
#[test]
fn a_despawned_source_leaves_no_node_and_no_layer() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    let kick = spawn(
        &mut app,
        ForkByClone(Pulse {
            from: 0,
            to: 48_000,
            level: 0.5,
        }),
    );
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let lfo = world.spawn(ModSource::new(Lfo::sine().hz(Hz(2.0)))).id();
    let follower = world.spawn(ModSource::new(Follower::new(kick))).id();
    world.spawn(ModRoute::new(lfo, cutoff).with_depth(Depth(0.1)));
    world.spawn(ModRoute::new(follower, cutoff).with_depth(Depth(-0.2)));
    settle(&mut app);
    render(&mut side, 1_024, 128);
    let state = app.world().resource::<AudioGraphRes>().modulation();
    let node = state
        .source_node(follower)
        .expect("the follower has a node");
    assert!(app
        .world()
        .resource::<AudioGraphRes>()
        .contains(AudioNode(node)));
    let (_, lane) = state.lane(cutoff).expect("a lane");
    assert_eq!(
        lane.program().inputs().len(),
        1,
        "precondition: it reads the follower"
    );

    app.world_mut().despawn(follower);
    settle(&mut app);
    render(&mut side, 1_024, 128);

    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert_eq!(state.source_node(follower), None, "nothing held for it");
    assert!(
        !app.world()
            .resource::<AudioGraphRes>()
            .contains(AudioNode(node)),
        "its node is gone"
    );
    let (_, lane) = state.lane(cutoff).expect("the LFO keeps the lane");
    let program = lane.program();
    assert!(program.inputs().is_empty(), "no input layer");
    assert_eq!(program.layers().len(), 1, "the LFO's layer");
}

/// **A knob written after a route edit is the knob.** The cutoff's
/// knob is the node's own cell, written through `AudioParam`; a depth
/// edit republishes the lane and writes no knob, so the knob written before
/// it and the one written after it are each the base the LFO rides on.
///
/// Mutations (run), each fails: the apply writing each range's bottom into
/// its knob (`set_knob(min)` after every apply, the old driver's reseed:
/// the cutoff rides on 0, frame 1056 reads 54.6); `write_param` dropping
/// its write (the knob stays 500).
#[test]
fn a_knob_written_after_a_route_edit_is_the_knob() {
    let (mut app, mut side) = app(1);
    app.add_audio_param::<Hz, { UnitParam::Cutoff as u16 }>();
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let lfo = world.spawn(ModSource::new(Lfo::sine().hz(Hz(2.0)))).id();
    let route = world
        .spawn(ModRoute::new(lfo, cutoff).with_depth(Depth(0.3)))
        .id();
    settle(&mut app);
    render(&mut side, 1_024, 128);

    app.world_mut()
        .entity_mut(probe)
        .insert(CutoffParam::new(Hz(300.0)));
    app.update();
    app.world_mut()
        .get_mut::<ModRoute>(route)
        .expect("the route")
        .depth = Depth(0.2);
    app.update();
    let a = render(&mut side, 2_048, 128);
    app.world_mut()
        .entity_mut(probe)
        .insert(CutoffParam::new(Hz(700.0)));
    app.update();
    let b = render(&mut side, 2_048, 128);

    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert_eq!(state.lane(cutoff).expect("a lane").1.knob(), Some(700.0));
    let tol = lfo_tolerance(0.2, 2.0);
    for (base, from, got) in [(300.0, 1_024, &a[0]), (700.0, 3_072, &b[0])] {
        for (i, &v) in got.iter().enumerate().skip(STEP) {
            let p = (from + i) as f64;
            let want = base + 200.0 * (std::f64::consts::TAU * 2.0 * p / RATE).sin();
            assert!(
                (f64::from(v) - want).abs() < tol,
                "knob {base}, frame {p}: {v} vs {want}"
            );
        }
    }
}

// ------------------------------------------------------- envelope --

/// **A held note envelope newly routed reads its level at once.** A note
/// held from frame 10 into a note envelope (sustain 0.4) routed to probe 1
/// (`0..=1`, knob 0.2, depth 0.5); probe 2 (knob 0.1) has a lane reading a
/// macro of 0. A route from the envelope to probe 2 is added in the
/// sustain: from the first grid piece after the frame it lands in, probe 2
/// reads `0.1 + 0.5 · 0.4`, not its knob until the next note.
///
/// Mutation (run, against the session's loop):
/// `Phase::AfterCommit` not sending the resyncs owed (`ModSession::resync`
/// skipped) fails: the resync stays owed. Neutral, not a gap: the
/// modulation phase asking for a commit on `Applied::resync` too (as it
/// once did) changes nothing, since `Phase::AfterCommit` sends the resyncs
/// owed whether or not the update commits.
#[test]
fn a_held_note_envelope_newly_routed_reads_its_level_at_once() {
    let (mut app, mut side) = app(2);
    let one = spawn(&mut app, ProbeUnit::new(0.2));
    let two = spawn(&mut app, ProbeUnit::new(0.1));
    let clip = spawn(&mut app, ForkByClone(Held { on: 10 }));
    master(&mut app, &[one, two]);
    let (c1, c2) = (
        ParamRef::unit(one, UnitParam::Cutoff),
        ParamRef::unit(two, UnitParam::Cutoff),
    );
    let adsr = Adsr {
        attack: Seconds(0.002),
        decay: Seconds(0.003),
        sustain: Amplitude(0.4),
        release: Seconds(0.2),
    };
    let world = app.world_mut();
    world.spawn(ModRange::new(c1, 0.0, 1.0));
    world.spawn(ModRange::new(c2, 0.0, 1.0));
    let env = world
        .spawn(ModSource::new(NoteEnvelope::new(clip, adsr)))
        .id();
    let zero = world.spawn(ModSource::new(Macro(0.0))).id();
    world.spawn(ModRoute::new(env, c1).with_depth(Depth(0.5)));
    world.spawn(ModRoute::new(zero, c2).with_depth(Depth(0.5)));
    settle(&mut app);
    let got = render_n(&mut side, 2, 4_096, 128);
    assert!(
        (got[0][4_095] - 0.4).abs() < 1e-6,
        "probe 1 in the sustain: {}",
        got[0][4_095]
    );
    assert!((got[1][4_095] - 0.1).abs() < 1e-7, "probe 2 at its knob");

    app.world_mut()
        .spawn(ModRoute::new(env, c2).with_depth(Depth(0.5)));
    app.update();
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .modulation()
            .resyncs_owed(),
        0,
        "the resync went with the frame's commit"
    );
    let got = render_n(&mut side, 2, 512, 128);
    for (f, &v) in got[1].iter().enumerate().skip(STEP) {
        assert!((v - 0.3).abs() < 1e-6, "frame {f}: {v}");
    }
}

// ----------------------------------------------------------- takes --

/// **A take suspends the automation, then restores it.** A cutoff (knob
/// 300) automated to 800: the automation plays. A touch take opens on the
/// automation's entity: the knob plays (the gesture drives the param).
/// The take records 650 and is released: its envelope becomes the
/// automation, and 650 plays.
///
/// Mutations (run), each fails: `Declared::collect` ignoring the take
/// (the automation plays through it: 800, not 300); `close_takes` leaving
/// the curve (the old 800 is back after the release, not 650);
/// `tutti_session::Take::is_done` ignoring a touch take's release (the
/// take stays open: 300; re-run against the session's `Take`).
#[test]
fn a_take_suspends_the_automation_then_restores_it() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(300.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut lane = AutomationEnvelope::new(0.0).with_range(0.0, 1_000.0);
    lane.add_point(AutomationPoint::new(0.0, 800.0));
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let automation = world.spawn(Automation::new(cutoff, Arc::new(lane))).id();
    settle(&mut app);
    let last = |out: &[Vec<f32>]| *out[0].last().expect("rendered");
    assert_eq!(
        last(&render(&mut side, 1_024, 128)),
        800.0,
        "the automation plays"
    );

    let mut take = AutomationTake::record(RecordMode::Touch, 0.0, 1_000.0);
    take.recorder_mut().touch(Beat(0.0), 650.0);
    app.world_mut().entity_mut(automation).insert(take);
    app.update();
    assert_eq!(
        last(&render(&mut side, 512, 128)),
        300.0,
        "the knob plays under the take"
    );

    app.world_mut()
        .get_mut::<AutomationTake>(automation)
        .expect("still open")
        .recorder_mut()
        .release(Beat(1.0), 650.0);
    app.update();
    assert!(
        app.world().get::<AutomationTake>(automation).is_none(),
        "the take closed"
    );
    assert_eq!(
        last(&render(&mut side, 512, 128)),
        650.0,
        "the take's envelope plays"
    );
}

// ---------------------------------------------------------- refusal --

/// **A follower downstream of its target is refused by route, and the
/// error is surfaced.** A follower of the probe's own output routed into
/// the probe's cutoff would be a cycle: the declaration is refused with the
/// route's entity, as a `ModulationRefused` message and in
/// `ModulationState::refusal`, and nothing is built. Dropping the route
/// applies the rest, and clears the refusal.
///
/// Mutations (run, against the session's loop), each fails: the adapter's
/// `run` not forwarding the session's
/// `ModulationRefused` (no message); `tutti_session::ModRefusal::route`
/// mapping `Cycle` to `None`; a refusal reported every update (the
/// session's `ModState::refuse` comparing to nothing: two messages across
/// two frames).
#[test]
fn a_follower_downstream_of_its_target_is_refused_by_route() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let lfo = world.spawn(ModSource::new(Macro(0.5))).id();
    world.spawn(ModRoute::new(lfo, cutoff).with_depth(Depth(0.2)));
    settle(&mut app);

    let follower = app
        .world_mut()
        .spawn(ModSource::new(Follower::new(probe)))
        .id();
    let route = app
        .world_mut()
        .spawn(ModRoute::new(follower, cutoff).with_depth(Depth(0.5)))
        .id();
    let read = |app: &mut App| {
        app.world_mut()
            .run_system_cached(|mut m: MessageReader<ModulationRefused>| {
                m.read().cloned().collect::<Vec<_>>()
            })
            .expect("runs")
    };
    read(&mut app);
    app.update();
    app.update();
    let refused = read(&mut app);
    assert_eq!(refused.len(), 1, "one message for one refusal: {refused:?}");
    assert_eq!(refused[0].route(), Some(route), "refused by route");
    assert!(matches!(refused[0].error, ReconcileError::Cycle { .. }));
    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert!(matches!(
        state.refusal(),
        Some(ReconcileError::Cycle { .. })
    ));
    assert_eq!(state.source_node(follower), None, "nothing built");
    let (_, lane) = state.lane(cutoff).expect("the lane before the refusal");
    assert!(lane.program().inputs().is_empty(), "unchanged");
    // The macro's `+100` still plays.
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 600.0);

    app.world_mut().despawn(route);
    settle(&mut app);
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .modulation()
            .refusal(),
        None
    );
}

/// **Two ranges, or two automations, on one param are refused by
/// target.** A macro's `+100` on a cutoff probe (knob 500, range
/// `0..=1000`). A second `ModRange` on the cutoff (`0..=2000`, which would
/// make it `+200`) is refused as `DuplicateTarget` naming the cutoff, once,
/// and nothing changes: the probe still plays 600. Without the refusal the
/// two would merge, the winner being whichever the query met last. Dropping
/// it applies again; then two `Automation`s on the cutoff are refused the
/// same way.
///
/// Mutations (run, against `tutti_session::declare_targets`, which
/// `Declared::collect` calls), each fails with no
/// message: a second range replacing the first (merged); a second
/// automation merged into the first (`automated.insert`'s answer
/// ignored).
#[test]
fn two_ranges_or_automations_on_one_param_are_refused() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, cutoff).with_depth(Depth(0.1)));
    settle(&mut app);
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 600.0);

    let read = |app: &mut App| {
        app.world_mut()
            .run_system_cached(|mut m: MessageReader<ModulationRefused>| {
                m.read().cloned().collect::<Vec<_>>()
            })
            .expect("runs")
    };
    let refused_by_cutoff = |app: &mut App, what: &str| {
        read(app);
        app.update();
        app.update();
        let refused = read(app);
        assert_eq!(refused.len(), 1, "{what}: one message: {refused:?}");
        assert_eq!(refused[0].target(), Some(cutoff), "{what}: by target");
        assert!(
            matches!(
                app.world()
                    .resource::<AudioGraphRes>()
                    .modulation()
                    .refusal(),
                Some(ReconcileError::Compile(
                    tutti_mod::CompileError::DuplicateTarget(_)
                ))
            ),
            "{what}: held"
        );
    };

    let second = app
        .world_mut()
        .spawn(ModRange::new(cutoff, 0.0, 2_000.0))
        .id();
    refused_by_cutoff(&mut app, "two ranges");
    assert_eq!(
        *render(&mut side, 1_024, 128)[0].last().unwrap(),
        600.0,
        "nothing changed"
    );
    app.world_mut().despawn(second);
    settle(&mut app);

    let curve = || Arc::new(AutomationEnvelope::new(0.0).with_range(0.0, 1_000.0));
    let world = app.world_mut();
    world.spawn(Automation::new(cutoff, curve()));
    world.spawn(Automation::new(cutoff, curve()));
    refused_by_cutoff(&mut app, "two automations");
}

// ----------------------------------------------------------- rebind --

/// **An entity rebound to a node with another param list keeps
/// modulating.** A macro's `+250` on a cutoff probe (knob 500); the entity
/// is crossfaded to a probe that declares the cutoff and a Q (another
/// param list, knob 400). After the re-apply that follows the commit
/// (doc 014 §8.2: the rebound target is unresolved until its node is in
/// a plan) the new node plays `400 + 250`, and a range edit after it lands
/// on the new node (`+500`).
///
/// Mutations (run, against the session's `Phase::AfterCommit`), each fails:
/// not re-applying while a target is unresolved
/// (`ModSession::reapply` returning `false`: the new node's first frame
/// plays its bare knob, 400, and only the next frame's apply resolves it);
/// the re-apply's commit skipped (`self.commit()` deleted: the lanes the
/// re-apply built wait for the next frame, so "modulated from its first
/// frame" fails).
#[test]
fn an_entity_rebound_to_another_param_list_keeps_modulating() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    let range = world.spawn(ModRange::new(cutoff, 0.0, 1_000.0)).id();
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, cutoff).with_depth(Depth(0.25)));
    settle(&mut app);
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 750.0, "precondition: modulated");

    crossfade_audio_node(
        &mut app.world_mut().commands(),
        probe,
        ProbeUnit::new(400.0).with_extra(UnitParam::Q),
    );
    // The frame that commits the new node re-applies after its commit: the
    // first block it plays is modulated.
    app.update();
    let out = render(&mut side, 1_024, 128);
    assert_eq!(
        *out[0].last().unwrap(),
        650.0,
        "the new node, modulated from its first frame"
    );
    settle(&mut app);
    let out = render(&mut side, 4_096, 128);
    assert_eq!(*out[0].last().unwrap(), 650.0, "the new node, modulated");

    app.world_mut()
        .get_mut::<ModRange>(range)
        .expect("the range")
        .max = 2_000.0;
    app.update();
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 900.0, "a later range edit lands");
}

// ------------------------------------------------- a host's commit --

/// A source declaring `LATE` frames of latency: a look-ahead's bookkeeping.
#[derive(Clone)]
struct Late;

const LATE: usize = 100;

impl tutti_graph::Node for Late {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(
            tutti_core::ChannelLayout::EMPTY,
            tutti_core::ChannelLayout::MONO,
        )
        .with_latency(tutti_types::Latency::new(tutti_types::Samples(LATE)))
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A host's staged edit committed on a frame with an unresolved target
/// publishes its PDC figures on that frame.** A host inserts, through
/// `AudioGraphRes` and between frames, a latent node on the outputs and a
/// probe it then modulates: the probe's target is unresolved when the
/// frame applies. The frame's commit sends the host's edit with the lanes,
/// and [`GraphLatency`](bevy_tutti::GraphLatency) is that plan's latency
/// on that frame; the re-apply after it resolves the target.
///
/// Not a mutation this can see (run): the session's
/// re-apply commit not publishing (`self.graph.commit()` for
/// `self.commit()`), or Bevy's `after_commit` not mirroring. The frame's
/// own commit takes every staged edit (`Graph::is_edited`),
/// so here the re-apply's plan adds a macro's lane, which moves no latency
/// figure; the old record (`after_commit` calling `graph.commit()`, not
/// `commit_and_publish`) already survived.
/// `a_re_apply_that_moves_the_latency_is_mirrored_on_that_frame` covers
/// both. The mutation that fails here: the frame's figures mirrored by
/// neither system (the `mirror` calls in `commit_graph` and `after_commit`
/// both deleted: `GraphLatency` reads 0).
#[test]
fn a_host_edit_committed_for_a_target_publishes_its_latency() {
    let (mut app, mut side) = app(1);
    settle(&mut app);
    let probe = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let (late, ()) = graph.insert(ForkByClone(Late));
        graph.set_outputs_from(late);
        let (probe, params) = graph.insert(ProbeUnit::new(500.0));
        graph.set_node_params(probe, Some(params));
        probe
    };
    let entity = app.world_mut().spawn(probe).id();
    let cutoff = ParamRef::unit(entity, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, cutoff).with_depth(Depth(0.1)));

    app.update();
    assert_eq!(
        app.world().resource::<bevy_tutti::GraphLatency>().0,
        tutti_types::Samples(LATE),
        "the plan the frame sent, published"
    );
    render(&mut side, 128, 128);
    settle(&mut app);
    assert!(app
        .world()
        .resource::<AudioGraphRes>()
        .modulation()
        .lane(cutoff)
        .is_some());
}

/// **A re-apply whose commit moves the latency is mirrored on that
/// frame.** A follower of a latent node (`LATE` frames, settled) is routed
/// to a probe spawned on output 0 in the same frame. The apply finds the
/// probe uncommitted, so the frame's commit sends the probe and the
/// follower with no lane (latency 0); `after_commit`'s re-apply builds the
/// lane, putting the follower's `LATE` frames in front of the probe, and
/// [`GraphLatency`](bevy_tutti::GraphLatency) reads them on that frame.
///
/// Mutations (run), each fails (`GraphLatency` reads 0): the session's
/// re-apply committing with `self.graph.commit()`, not `self.commit()` (no
/// publish); `after_commit` not mirroring (its `mirror` deleted).
#[test]
fn a_re_apply_that_moves_the_latency_is_mirrored_on_that_frame() {
    let (mut app, mut side) = app(1);
    let late = spawn(&mut app, ForkByClone(Late));
    settle(&mut app);
    render(&mut side, 128, 128);

    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let follower = world.spawn(ModSource::new(Follower::new(late))).id();
    world.spawn(ModRoute::new(follower, cutoff).with_depth(Depth(0.5)));
    app.update();
    assert_eq!(
        app.world().resource::<bevy_tutti::GraphLatency>().0,
        tutti_types::Samples(LATE),
        "the re-apply's plan, mirrored"
    );
    assert!(app
        .world()
        .resource::<AudioGraphRes>()
        .modulation()
        .lane(cutoff)
        .is_some());
}

// ------------------------------------- ported from the old driver --
//
// Doc 014 §10 deleted the frame-rate driver (`modulation::legacy`) and its
// suites (`mod_value_path.rs`, `mod_audio_rate.rs`,
// `graph_value_modulation.rs`). The properties below were theirs and still
// hold of the declarations; each is asserted on the lanes now.

/// The value probe 0 settles to, past the connect's declick: frames
/// `DECLICK + STEP..` of a 1024-frame render must all read it.
fn assert_holds(out: &[f32], want: f32, what: &str) {
    for (f, &v) in out.iter().enumerate().skip(DECLICK + STEP) {
        assert!(
            (v - want).abs() < 1e-3,
            "{what}: frame {f} reads {v}, not {want}"
        );
    }
}

/// **Two routes onto one param sum, and a disabled route adds nothing.**
/// Two macros at 1 on a cutoff (knob 500 in `0..=1000`) at depths 0.1 and
/// 0.2 add 100 and 200: 800. Disabling the second (`enabled: false`) leaves
/// 600; enabling it again, 800.
///
/// Ported from `mod_value_path.rs`'s
/// `two_routes_onto_one_param_sum_instead_of_overwriting` and
/// `a_disabled_route_contributes_nothing`.
///
/// Mutations (run), each fails: `Declared::collect` ignoring
/// `enabled` (the disabled route still adds: 800 for 600); the lane's
/// program keeping only its last layer (`ControlProgram::new` given
/// `layers.split_off(len - 1)`: 700 for 800).
#[test]
fn two_routes_on_one_param_sum_and_a_disabled_one_adds_nothing() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let a = world.spawn(ModSource::new(Macro(1.0))).id();
    let b = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(a, cutoff).with_depth(Depth(0.1)));
    let second = world
        .spawn(ModRoute::new(b, cutoff).with_depth(Depth(0.2)))
        .id();
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "both routes");

    app.world_mut()
        .get_mut::<ModRoute>(second)
        .expect("the route")
        .enabled = false;
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 600.0, "one disabled");

    app.world_mut()
        .get_mut::<ModRoute>(second)
        .expect("the route")
        .enabled = true;
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "enabled again");
}

/// **Removing the last route returns the param to its knob.** A macro at
/// depth 0.1 on a cutoff (knob 500 in `0..=1000`) adds 100: 600. With the
/// route despawned the target has nothing left to drive it, so its lane is
/// gone and the probe reads the knob, 500, past the disconnect's declick.
/// Every other removal here leaves a route on the target; this is the one
/// where nothing does.
///
/// Ported from `mod_value_path.rs`'s
/// `removing_a_route_returns_the_param_to_its_base`.
///
/// Mutation (run): `Modulation::apply`'s lane sweep keeping a lane
/// whose target has no bind left (the `gone` filter's
/// `binds.get(t).is_none_or(..)` as `is_some_and(..)`) → the lane stays on
/// the port with its stale program → fails.
#[test]
fn removing_the_last_route_returns_the_param_to_its_knob() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    let route = world
        .spawn(ModRoute::new(offset, cutoff).with_depth(Depth(0.1)))
        .id();
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 600.0, "the route");
    assert!(
        app.world()
            .resource::<AudioGraphRes>()
            .modulation()
            .lane(cutoff)
            .is_some(),
        "the route has its lane"
    );

    app.world_mut().despawn(route);
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 500.0, "no route left");
    assert!(
        app.world()
            .resource::<AudioGraphRes>()
            .modulation()
            .lane(cutoff)
            .is_none(),
        "and no lane"
    );
}

/// A host's own source kind: a macro at half its setting.
struct HalfMacro(f32);

impl bevy_tutti::modulation::ModSourceKind for HalfMacro {
    fn declare(&self) -> tutti_mod::Source {
        tutti_mod::Source::Macro {
            value: self.0 / 2.0,
        }
    }
}

/// **A host's own source kind drives a param, its config included.** A
/// `HalfMacro(1.0)` (raw 0.5) routed at depth 0.4 onto a cutoff (knob 500
/// in `0..=1000`) adds 200, beside a built-in macro at depth 0.1 (100):
/// 800. Replacing its component with `HalfMacro(-1.0)` moves it to 400.
///
/// Ported from `mod_value_path.rs`'s `a_custom_kind_drives_a_param`,
/// `the_kinds_own_config_reaches_the_modulator` and
/// `a_built_in_and_a_custom_kind_coexist`. (`registering_a_kind_twice_is_
/// idempotent` went with the registration: a `ModSource` holds its kind,
/// and nothing is registered.)
///
/// Mutation (run): `Declared::collect` declaring every source as a
/// macro at 1 (`Source::Macro { value: 1.0 }` for `s.kind.declare()`) →
/// 900 → fails.
#[test]
fn a_hosts_own_source_kind_drives_a_param() {
    let (mut app, mut side) = app(1);
    let probe = spawn(&mut app, ProbeUnit::new(500.0));
    master(&mut app, &[probe]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let half = world.spawn(ModSource::new(HalfMacro(1.0))).id();
    let built_in = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(half, cutoff).with_depth(Depth(0.4)));
    world.spawn(ModRoute::new(built_in, cutoff).with_depth(Depth(0.1)));
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "the host's kind");

    app.world_mut()
        .entity_mut(half)
        .insert(ModSource::new(HalfMacro(-1.0)));
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 400.0, "its config moved");
}

/// **A route declared before its target's node binds once the node is
/// in.** The range and the route name an entity with no node yet: nothing
/// is refused, and no lane is built. Once the entity gets its node
/// (`insert_audio_node`), the cutoff (knob 500 in `0..=1000`) reads the
/// macro's `+300`.
///
/// Ported from `mod_audio_rate.rs`'s
/// `a_route_declared_before_its_sinks_node_still_binds`.
///
/// Mutation (run): `Modulation::apply` refusing a target it cannot
/// resolve (`WrongTarget` for `unresolved_targets.push`) → the nodeless
/// target is refused → fails.
#[test]
fn a_route_declared_before_its_targets_node_binds_once_it_is_in() {
    use bevy_tutti::graph::InsertAudioNode;

    let (mut app, mut side) = app(1);
    let entity = app.world_mut().spawn_empty().id();
    let cutoff = ParamRef::unit(entity, UnitParam::Cutoff);
    let world = app.world_mut();
    world.spawn(ModRange::new(cutoff, 0.0, 1_000.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, cutoff).with_depth(Depth(0.3)));
    for _ in 0..3 {
        app.update();
    }
    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert_eq!(state.refusal(), None, "a target with no node is no error");
    assert!(state.lane(cutoff).is_none(), "and has no lane");

    app.world_mut()
        .commands()
        .entity(entity)
        .insert_audio_node(ProbeUnit::new(500.0));
    app.world_mut().flush();
    master(&mut app, &[entity]);
    settle(&mut app);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "bound late");
}

/// **A bus strip's volume and pan take lanes.** The strip declares its
/// params itself (`STRIP_PARAMS`), so a route onto either resolves to a
/// lane on its port, with nothing refused.
///
/// Ported from `mod_audio_rate.rs`'s
/// `a_bus_strips_volume_and_pan_reach_audio_rate` (its audio-rate half
/// went with `ParamFrom::Audio`, doc 014 §11).
///
/// Mutation (run): `Pan` dropped from `STRIP_PARAMS` → the route is
/// refused (`NoSuchParam`) → fails.
#[test]
fn a_bus_strips_volume_and_pan_take_lanes() {
    let (mut app, _side) = app(2);
    let strip = spawn(
        &mut app,
        tutti_nodes::BusStripNode::with_channels(tutti_types::ChannelLayout::STEREO),
    );
    let offset = app.world_mut().spawn(ModSource::new(Macro(0.5))).id();
    for param in [UnitParam::Volume, UnitParam::Pan] {
        let at = ParamRef::unit(strip, param);
        let world = app.world_mut();
        world.spawn(ModRange::new(at, 0.0, 1.0));
        world.spawn(ModRoute::new(offset, at).with_depth(Depth(0.5)));
    }
    settle(&mut app);
    let state = app.world().resource::<AudioGraphRes>().modulation();
    for param in [UnitParam::Volume, UnitParam::Pan] {
        assert!(
            state.lane(ParamRef::unit(strip, param)).is_some(),
            "{param:?} has its lane"
        );
    }
}

/// **A chorus's rate and a phaser's mix take lanes.** Their params were
/// knobs the old frame-rate driver wrote; the nodes now declare them as
/// ports (`MOD_DELAY_PARAMS`, `PHASER_PARAMS`), so a route onto each resolves to a lane, with nothing
/// refused. What a lane on them sounds like is pinned where the nodes live
/// (`tutti-nodes`' `a_lane_modulates_a_chorus_rate_around_its_own_cell`
/// and `each_fed_param_is_the_one_the_dsp_reads`, `tutti-polysynth`'s
/// `param_lanes.rs`).
///
/// Mutation (run): `ModDelayNode` not declaring its ports (the
/// `.with_params(&MOD_DELAY_PARAMS)` dropped) → the chorus route is refused
/// (`NoSuchParam`) → fails.
#[test]
fn a_chorus_rate_and_a_phaser_mix_take_lanes() {
    let (mut app, _side) = app(2);
    let chorus = spawn(
        &mut app,
        tutti_nodes::ModDelayNode::chorus(tutti_types::ChannelLayout::STEREO),
    );
    let phaser = spawn(
        &mut app,
        tutti_nodes::PhaserNode::with_channels(tutti_types::ChannelLayout::STEREO, 4),
    );
    let offset = app.world_mut().spawn(ModSource::new(Macro(0.5))).id();
    let targets = [
        ParamRef::unit(chorus, UnitParam::Rate),
        ParamRef::unit(phaser, UnitParam::Wet),
    ];
    for at in targets {
        let world = app.world_mut();
        world.spawn(ModRange::new(at, 0.0, 1.0));
        world.spawn(ModRoute::new(offset, at).with_depth(Depth(0.5)));
    }
    settle(&mut app);
    let state = app.world().resource::<AudioGraphRes>().modulation();
    for at in targets {
        assert!(state.lane(at).is_some(), "{at:?} has its lane");
    }
}

/// **A synth spawned as a graph node is a modulation target.** A
/// `PolySynth` goes in through `spawn_audio_node` like any node; a route
/// into its volume resolves (nothing refused, nothing unresolved), and the
/// volume's lane has the synth's own volume cell as its knob.
///
/// Mutation (run): the synth registered with no params (the session's
/// `impl SessionNode for PolySynth {}` in place of `param_session_node!`)
/// → the lane has no knob (the synth's volume cell is not reached) → fails.
/// Mutation (run): the synth's `ParamSet` handing `Volume` a fresh cell
/// rather than the synth's own → the knob does not follow a store into the
/// synth's volume → fails.
#[cfg(feature = "synth")]
#[test]
fn a_spawned_synth_is_a_modulation_target() {
    use std::sync::atomic::Ordering;
    use tutti_polysynth::{PolySynth, SynthConfig};

    let (mut app, _side) = app(2);
    let node = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let cell = node.volume_atomic();
    let synth = spawn(&mut app, node);
    master(&mut app, &[synth, synth]);
    let volume = ParamRef::unit(synth, UnitParam::Volume);
    let world = app.world_mut();
    world.spawn(ModRange::new(volume, 0.0, 1.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, volume).with_depth(Depth(0.25)));
    settle(&mut app);
    let state = app.world().resource::<AudioGraphRes>().modulation();
    let (_, lane) = state.lane(volume).expect("the volume has a lane");
    // The knob is read from its cell: a store into the synth's own volume
    // cell is what the lane reads, value for value.
    for v in [0.37, 0.81] {
        cell.store(v, Ordering::Release);
        assert_eq!(lane.knob(), Some(v), "its knob is the synth's volume cell");
    }
}

//! The modulation properties that need the plan the graph sent
//! (`AudioGraphRes::sent_plan`, crate-private): a program edit sends none,
//! a new signal node sends one. The rest are `tests/mod_lanes.rs`.

use std::sync::Arc;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use tutti_core::transport::Transport;
use tutti_graph::ForkByClone;
use tutti_types::{Beat, Bpm, Depth, Hz, SampleRate, UnitParam};

use super::{Lfo, ModRange, ModRoute, Mods, ParamRef, TuttiModulationPlugin};
use crate::graph::{
    AudioGraphRes, AudioSide, GraphReconcilePlugin, MasterSources, PortSource, SpawnAudioNode,
    TransportRes,
};
use crate::AudioEngineState;

#[path = "../../tests/common/probe.rs"]
#[allow(dead_code)]
mod probe;
crate::param_graph_node!(probe::ProbeUnit);

use probe::{ProbeUnit, Pulse};

const RATE: f64 = 48_000.0;
/// A lane's control step.
const STEP: usize = 32;
/// A port's connect crossfade.
const DECLICK: usize = 256;
const BLOCK: usize = 128;

fn app() -> (App, AudioSide) {
    let mut graph = AudioGraphRes::headless(0, 1);
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

fn spawn<N>(app: &mut App, node: N) -> Entity
where
    N: crate::graph::SessionNode,
    N::Controls: Send + Sync + 'static,
{
    let entity = app.world_mut().commands().spawn_audio_node(node).id();
    app.world_mut().flush();
    entity
}

/// `frames` frames of output 0, stopped (an `Hz` LFO runs on the play
/// clock, which runs while stopped).
fn render(side: &mut AudioSide, frames: usize) -> Vec<f32> {
    let mut out = vec![Vec::new()];
    side.set_transport(tutti_graph::Transport::new(
        false,
        Bpm(120.0),
        Beat(0.0),
        None,
    ));
    side.render(frames, BLOCK, &mut out);
    out.swap_remove(0)
}

/// One frame, then a block rendered, as a device would between frames.
fn frame(app: &mut App, side: &mut AudioSide) {
    app.update();
    render(side, BLOCK);
}

fn sent(app: &App) -> Arc<tutti_graph::Plan> {
    app.world()
        .resource::<AudioGraphRes>()
        .sent_plan()
        .expect("a plan was sent")
}

fn idle(app: &App) -> bool {
    !app.world().resource::<AudioGraphRes>().is_edited()
}

/// A cutoff probe (knob 500, range `0..=1000`) on output 0, a sine at
/// `rate` Hz routed into it at `depth`, settled. Returns the probe, the
/// range's entity and the route's.
fn lfo_scene(app: &mut App, side: &mut AudioSide, rate: f32, depth: f32) -> [Entity; 3] {
    let filter = spawn(app, ProbeUnit::new(500.0));
    app.insert_resource(MasterSources::default().with(0, PortSource::node(filter)));
    let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
    let [range, route] = app
        .world_mut()
        .run_system_cached_with(
            |In((cutoff, rate, depth)): In<(ParamRef, f32, f32)>, mut mods: Mods| {
                let range = mods.range(cutoff, 0.0, 1_000.0);
                let lfo = mods.lfo(Lfo::sine().hz(Hz(rate)));
                [range, mods.route(lfo, cutoff, Depth(depth))]
            },
            (cutoff, rate, depth),
        )
        .expect("runs");
    for _ in 0..4 {
        frame(app, side);
    }
    let state = app.world().resource::<AudioGraphRes>().modulation();
    assert_eq!(state.refusal(), None);
    assert!(state.lane(cutoff).is_some(), "precondition: a lane");
    [filter, range, route]
}

/// **A range or depth edit sends no plan; a follower route sends one.**
/// Doc 014 §8.2: a program edit is a store into the lane (the old driver
/// rebuilt per-route nodes); a follower is a node, so routing one is one
/// commit, and the frames after it send nothing more.
///
/// Each edit is checked to have reached the render, so "no plan" is not an
/// edit that did nothing.
///
/// Mutations (run), each fails: a lane rebuilt when its program changes
/// (`Modulation::apply`'s gone-lanes filter also taking
/// `compiled.programs[t] != lane.program`: "a depth edit sent a plan"); a
/// follower rebuilt on every apply (`p.decl != n.decl` as `==`: more than
/// one plan); the range store skipped (`if lane.range != range` as
/// `false`: "the range clamps" reads 500). Not a mutation this can see:
/// the session's `Phase::AfterCommit` committing every update, since a
/// commit with nothing staged sends no plan.
#[test]
fn range_and_depth_edits_send_no_plan_and_a_follower_route_one_commit() {
    let (mut app, mut side) = app();
    // A rate low enough that one block barely moves it: the render after
    // an edit is read against the edit, not the wave.
    let [_, range, route] = lfo_scene(&mut app, &mut side, 0.5, 0.3);
    let settled = sent(&app);

    // The depth: 0.3 → 0.0, the knob alone.
    app.world_mut().get_mut::<ModRoute>(route).unwrap().depth = Depth(0.0);
    for _ in 0..3 {
        frame(&mut app, &mut side);
        assert!(
            Arc::ptr_eq(&sent(&app), &settled),
            "a depth edit sent a plan"
        );
        assert!(idle(&app), "a depth edit staged an edit");
    }
    let v = render(&mut side, BLOCK);
    assert!(v.iter().all(|&x| x == 500.0), "depth 0 is the knob: {v:?}");

    // The range: clamped to 600..=700, the knob (500) reads 600.
    {
        let mut r = app.world_mut().get_mut::<ModRange>(range).unwrap();
        r.min = 600.0;
        r.max = 700.0;
    }
    for _ in 0..3 {
        frame(&mut app, &mut side);
        assert!(
            Arc::ptr_eq(&sent(&app), &settled),
            "a range edit sent a plan"
        );
        assert!(idle(&app), "a range edit staged an edit");
    }
    let v = render(&mut side, BLOCK);
    assert!(v.iter().all(|&x| x == 600.0), "the range clamps: {v:?}");
    // And back to 0..=1000 (a route's depth is a fraction of the span).
    {
        let mut r = app.world_mut().get_mut::<ModRange>(range).unwrap();
        r.min = 0.0;
        r.max = 1_000.0;
    }
    frame(&mut app, &mut side);
    assert!(
        Arc::ptr_eq(&sent(&app), &settled),
        "a range edit sent a plan"
    );

    // A follower on a pulse, routed in: the pulse node first, on its own.
    let pulse = spawn(
        &mut app,
        ForkByClone(Pulse {
            from: 0,
            to: u64::MAX,
            level: 0.8,
        }),
    );
    for _ in 0..3 {
        frame(&mut app, &mut side);
    }
    let before = sent(&app);
    let filter = app.world().get::<ModRoute>(route).unwrap().target;
    app.world_mut()
        .run_system_cached_with(
            |In((pulse, cutoff)): In<(Entity, ParamRef)>, mut mods: Mods| {
                let kick = mods.follower(pulse);
                mods.route(kick, cutoff, Depth(0.15));
            },
            (pulse, filter),
        )
        .expect("runs");
    let mut plans = vec![before];
    for _ in 0..6 {
        frame(&mut app, &mut side);
        let now = sent(&app);
        if !Arc::ptr_eq(plans.last().unwrap(), &now) {
            plans.push(now);
        }
    }
    assert_eq!(plans.len(), 2, "a follower route sends exactly one plan");
    assert!(idle(&app));
    // And it modulates: the knob 500 plus the follower's 0.8 · 0.15 · 1000,
    // 620, inside 600..=700.
    let v = render(&mut side, 4_800);
    let last = *v.last().unwrap();
    assert!((last - 620.0).abs() < 0.5, "the follower raises it: {last}");
}

/// **A depth slider keeps the LFO's phase.** Dragging a route's depth
/// is a store into the lane's program: the wave goes on from where it was,
/// at the new depth, on the frame the next block starts. The old driver
/// rebuilt the source's node on a depth change, restarting the wave.
///
/// The edit lands at a block boundary; every frame after it reads
/// `500 + 1000 · 0.1 · sin(2π · 4 · t)` with `t` counted from frame 0,
/// within the control grid's bound.
///
/// Mutation (run): a lane rebuilt when its program changes (the
/// gone-lanes filter in `Modulation::apply` also taking a changed program)
/// fails at "a depth edit sent a plan", and with that assertion taken out,
/// at the render too (frame 1056 reads 640.1, the new lane's connect
/// crossfade from the old one, against 552.5).
#[test]
fn a_depth_slider_keeps_phase() {
    let (mut app, mut side) = app();
    let rate = 4.0;
    let [_, _, route] = lfo_scene(&mut app, &mut side, rate, 0.3);
    let settled = sent(&app);
    // `lfo_scene` rendered its frames' blocks: the clock is past them.
    let start = 4 * BLOCK;
    let before = render(&mut side, 2 * DECLICK);
    let value = |depth: f64, f: usize| {
        500.0 + 1000.0 * depth * (std::f64::consts::TAU * rate as f64 * f as f64 / RATE).sin()
    };
    let h = std::f64::consts::TAU * f64::from(rate) * STEP as f64 / RATE;
    let tol = |depth: f64| h * h / 8.0 * 1000.0 * depth + 1e-3;
    for (i, &v) in before.iter().enumerate().skip(DECLICK) {
        let f = start + i;
        let want = value(0.3, f);
        assert!(
            (f64::from(v) - want).abs() < tol(0.3),
            "before, frame {f}: {v} vs {want}"
        );
    }

    app.world_mut().get_mut::<ModRoute>(route).unwrap().depth = Depth(0.1);
    app.update();
    assert!(
        Arc::ptr_eq(&sent(&app), &settled),
        "a depth edit sent a plan"
    );
    let after = render(&mut side, 12_000);
    let start = start + before.len();
    for (i, &v) in after.iter().enumerate().skip(STEP) {
        let f = start + i;
        let want = value(0.1, f);
        assert!(
            (f64::from(v) - want).abs() < tol(0.1),
            "after, frame {f}: {v} vs {want}"
        );
    }
}

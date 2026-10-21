//! The adapter's halves of the graph tests whose session halves live in
//! `tutti-session`: what `spawn_audio_node`, `AudioParam` and
//! `crossfade_audio_node` bind to an entity, on a session that already
//! passes its own.

use bevy_app::prelude::*;
use tutti_core::{AudioNode, ChannelLayout, Drive, UnitParam};
use tutti_graph::{IntoNode, NodeParts, ParamSet, Prepare};

use crate::graph::{
    AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, MasterSources, PortSource,
    SpawnAudioNode,
};
use crate::AudioEngineState;

type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

/// A source whose one output is its `Drive` param — the shape of every
/// node whose param a host drives (`DistortionNode`'s drive, a filter's
/// cutoff): its drive a `Param` cell addressed by a `ParamSet`, inserted
/// through `param_parts` (so its fork starts from the values its cells
/// hold) and spawned as a `SessionNode` with its params.
#[derive(Clone)]
struct Knob {
    drive: tutti_types::Param<Drive>,
}

impl Knob {
    const BUILT_WITH: f32 = 1.0;

    fn new() -> Self {
        Self::at(Self::BUILT_WITH)
    }

    fn at(drive: f32) -> Self {
        Self {
            drive: tutti_types::Param::new(Drive(drive)),
        }
    }
}

impl tutti_graph::Node for Knob {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_params(&[UnitParam::Drive])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        // The drive's port under modulation (a lane's values), else
        // its cell.
        match io.param(0).frames() {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => io.output(0).fill(self.drive.load().get()),
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then(|| self.drive.load().get())
    }
}

impl tutti_graph::ParamNode for Knob {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Drive, self.drive.as_atomic())
            .build()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.drive.detach();
        fork
    }
}

impl IntoNode for Knob {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl crate::graph::SessionNode for Knob {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// **Every param write path reaches a fork of a node** — the constraint
/// export by `Editor::fork` rests on — through the node's `ParamSet`:
/// `AudioParam` and `set_param` write the node's cell, and a fork starts
/// from the values its cells hold. A modulated param
/// is written the same way (its cell is its lane's knob: one writer,
/// doc 014 §8.2), and its lane is a graph node, so the fork renders the
/// same modulation as live.
///
/// The paths, one knob each, rendered on one global output each:
/// `AudioParam` on an unmodulated param (`write_param` → `set_param`);
/// `AudioGraphRes::set_param` directly; `AudioParam` on a modulated param
/// (the knob under a macro's `+2`); and a modulated param with no write
/// (the knob it was built with, under the same `+2`).
///
/// The session's own half (`tutti-session`'s test of the same name) pins
/// `set_param` and `fork` on a `Session`; this half pins the adapter's
/// paths into them: the spawn, the `AudioParam` component and the
/// modulation lanes.
///
/// Mutations (run; each fails its channel):
/// - `insert_and_bind` inserting with `AudioGraphRes::insert`, not `add`
///   (the params never addressed) → channels 0 and 1 stay at 1;
/// - `write_param` not calling `set_param` → channel 0 stays at 1;
/// - (`modulation`) `Declared`'s `ModKeys::node` answering `None` → the
///   lanes have no target, and channels 2 and 3 render the knob alone.
#[test]
fn every_param_write_path_reaches_a_fork() {
    let mut graph = AudioGraphRes::headless(0, 4);
    graph.set_sample_rate(tutti_core::SampleRate(48_000.0));
    let mut app = App::new();
    app.insert_resource(graph);
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    #[cfg(feature = "modulation")]
    {
        app.insert_resource(crate::graph::TransportRes(
            tutti_core::transport::Transport::new(48_000.0),
        ));
        app.add_plugins(crate::modulation::TuttiModulationPlugin);
    }
    app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

    let mut commands = app.world_mut().commands();
    let knobs = [(); 4].map(|()| commands.spawn_audio_node(Knob::new()).id());
    commands.insert_resource(
        MasterSources::default()
            .with(0, PortSource::node(knobs[0]))
            .with(1, PortSource::node(knobs[1]))
            .with(2, PortSource::node(knobs[2]))
            .with(3, PortSource::node(knobs[3])),
    );
    app.world_mut().flush();
    app.update();

    app.world_mut()
        .entity_mut(knobs[0])
        .insert(DriveParam::new(Drive(4.0)));
    let node = *app.world().get::<AudioNode>(knobs[1]).unwrap();
    app.world_mut()
        .resource_mut::<AudioGraphRes>()
        .set_param(node, UnitParam::Drive, 5.0);
    #[cfg(feature = "modulation")]
    {
        use crate::modulation::{Macro, ModRange, ModRoute, ModSource, ParamRef};
        use tutti_types::Depth;
        // A macro at 1 by a fifth of `0..10`: `+2` on the knob.
        let world = app.world_mut();
        let offset = world.spawn(ModSource::new(Macro(1.0))).id();
        for &knob in &knobs[2..] {
            let drive = ParamRef::unit(knob, UnitParam::Drive);
            world.spawn(ModRange::new(drive, 0.0, 10.0));
            world.spawn(ModRoute::new(offset, drive).with_depth(Depth(0.2)));
        }
        app.update();
        app.world_mut()
            .entity_mut(knobs[2])
            .insert(DriveParam::new(Drive(6.0)));
    }
    app.update();
    app.update();

    // Past the lanes' connect declick (256 frames).
    let mut live = [0.0f32; 4];
    for _ in 0..512 {
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .render_frame(&mut live);
    }
    assert_eq!(live[0], 4.0, "an AudioParam write reaches the live node");
    assert_eq!(live[1], 5.0, "a set_param write reaches the live node");
    let graph = app.world().resource::<AudioGraphRes>();
    let mut fork = graph.fork().expect("every knob is forkable");
    let mut forked = [0.0f32; 4];
    fork.tick(&[], &mut forked);
    assert_eq!(forked[0], 4.0, "an AudioParam write reaches the fork");
    assert_eq!(forked[1], 5.0, "a set_param write reaches the fork");
    #[cfg(feature = "modulation")]
    {
        assert_eq!(live[2], 8.0, "the knob written under its modulation, live");
        assert_eq!(forked[2], 8.0, "and in the fork");
        assert_eq!(
            live[3],
            Knob::BUILT_WITH + 2.0,
            "the knob built under its modulation, live"
        );
        assert_eq!(forked[3], Knob::BUILT_WITH + 2.0, "and in the fork");
    }
}

/// **A crossfade asked for while the graph re-prepares waits, lands once
/// it resumes, and swaps the entity's controls only then** — the graph an
/// engine runs (the audio side is taken) changing rate, and a crossfade
/// before the executor has handed its units back.
///
/// The session holds the node and lands it from its `Collect` phase (its
/// own half, `tutti-session`'s
/// `a_crossfade_refused_busy_during_a_re_prepare_lands_on_a_later_update`);
/// this half pins the adapter's binding: binding the new controls at
/// request time would leave the entity steering a unit that is not playing
/// (or, on a poisoned graph, never will).
///
/// Mutations (run):
/// - `land` dropping the binding on `Waiting` instead of parking it →
///   nothing waits in `PendingCrossfades` → fails;
/// - `PendingCrossfades::resolve` not calling the binding once the node
///   lands → the entity no longer steers the incoming unit → fails at the
///   last seed.
#[test]
fn a_crossfade_during_a_re_prepare_lands_after_it() {
    let mut graph = AudioGraphRes::headless(0, 1);
    graph.set_sample_rate(tutti_core::SampleRate(48_000.0));
    let mut side = graph.take_audio_side();
    let mut app = App::new();
    app.insert_resource(graph);
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    #[cfg(feature = "modulation")]
    {
        app.insert_resource(crate::graph::TransportRes(
            tutti_core::transport::Transport::new(48_000.0),
        ));
        app.add_plugins(crate::modulation::TuttiModulationPlugin);
    }
    let knob = app
        .world_mut()
        .commands()
        .spawn_audio_node(Knob::new())
        .id();
    app.world_mut()
        .commands()
        .insert_resource(MasterSources::default().with(0, PortSource::node(knob)));
    app.world_mut().flush();
    app.update();
    let mut out = [0.0f32];
    side.tick(&[], &mut out);
    assert_eq!(out[0], Knob::BUILT_WITH, "the knob plays");

    // The rate changes on the running graph: the first half is sent, and
    // the executor has not run it yet.
    app.world_mut()
        .resource_mut::<AudioGraphRes>()
        .set_sample_rate(tutti_core::SampleRate(44_100.0));
    crate::graph::crossfade_audio_node(&mut app.world_mut().commands(), knob, Knob::at(3.0));
    app.world_mut().flush();
    assert_eq!(
        app.world()
            .resource::<crate::graph::PendingCrossfades>()
            .len(),
        1,
        "the crossfade waits for the re-prepare"
    );
    // Frame by frame through the re-prepare: the executor checks its units
    // out (a silent block), the editor sends them back, the executor
    // resumes. The crossfade still waits until a frame finds the graph
    // resumed.
    app.update();
    side.tick(&[], &mut out);
    app.update();
    // Still the old unit's controls: seed its cell through the entity's
    // handle, and the resumed old knob plays the seed.
    seed(&app, knob, 7.0);
    side.tick(&[], &mut out);
    assert_eq!(out[0], 7.0, "the entity still steers the playing unit");

    app.update();
    assert!(
        app.world()
            .resource::<crate::graph::PendingCrossfades>()
            .is_empty(),
        "landed once the graph resumed"
    );
    for _ in 0..512 {
        side.tick(&[], &mut out);
    }
    assert_eq!(out[0], 3.0, "the incoming knob is what sounds");
    // Its controls are the entity's now.
    seed(&app, knob, 8.0);
    side.tick(&[], &mut out);
    assert_eq!(out[0], 8.0, "the entity steers the incoming unit");
}

/// A node that cannot run above 90 kHz: its `prepare` panics there,
/// which is how a re-prepare poisons an editor.
struct Grenade;

impl tutti_graph::Node for Grenade {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, prepare: &Prepare) {
        assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
    }
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

/// **On a poisoned graph a crossfade is refused and logged**: nothing is
/// parked (no frame will ever take it) and the entity's controls are left
/// as they were — the incoming unit's are not bound to a node it never
/// reached.
///
/// Mutation (run): the session's `replace` answering `Busy` on a poisoned
/// graph → the session parks the node and this crate parks its binding, for
/// good → fails.
#[test]
fn a_crossfade_on_a_poisoned_graph_is_refused_and_keeps_the_controls() {
    let mut graph = AudioGraphRes::headless(0, 1);
    graph.set_sample_rate(tutti_core::SampleRate(48_000.0));
    let mut app = App::new();
    app.insert_resource(graph);
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    #[cfg(feature = "modulation")]
    {
        app.insert_resource(crate::graph::TransportRes(
            tutti_core::transport::Transport::new(48_000.0),
        ));
        app.add_plugins(crate::modulation::TuttiModulationPlugin);
    }
    let mut commands = app.world_mut().commands();
    let knob = commands.spawn_audio_node(Knob::new()).id();
    commands.spawn_audio_node(tutti_graph::Unforkable(Grenade));
    app.world_mut().flush();
    app.update();
    // Both halves run here (the executor is local); the second panics in
    // the grenade's `prepare` and poisons the editor.
    app.world_mut()
        .resource_mut::<AudioGraphRes>()
        .set_sample_rate(tutti_core::SampleRate(96_000.0));
    let before = handle_ptr(&app, knob);

    crate::graph::crossfade_audio_node(&mut app.world_mut().commands(), knob, Knob::at(3.0));
    app.world_mut().flush();
    app.update();
    assert!(
        app.world()
            .resource::<crate::graph::PendingCrossfades>()
            .is_empty(),
        "a poisoned graph never takes it, so nothing waits"
    );
    assert_eq!(handle_ptr(&app, knob), before, "the controls stayed put");
}

/// The address of `entity`'s `Drive` cell, through the controls bound
/// to it, to tell one node's controls from another's.
fn handle_ptr(app: &App, entity: bevy_ecs::entity::Entity) -> *const () {
    let controls = app
        .world()
        .get::<crate::graph::NodeControls<ParamSet>>(entity)
        .expect("a knob's controls are bound");
    let cell = controls.0.cell(UnitParam::Drive).expect("a knob has Drive");
    std::sync::Arc::as_ptr(&cell).cast()
}

/// Write `entity`'s `Drive` through the controls bound to it: the cell
/// of whichever node they came from.
fn seed(app: &App, entity: bevy_ecs::entity::Entity, value: f32) {
    let controls = app
        .world()
        .get::<crate::graph::NodeControls<ParamSet>>(entity)
        .expect("a knob's controls are bound");
    assert!(controls.0.set(UnitParam::Drive, value), "a knob has Drive");
}

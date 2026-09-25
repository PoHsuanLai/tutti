//! Every insertion path captures a unit's controls before the unit enters the
//! graph, and the readers ignore a capture that belongs to another node.
//!
//! `bevy_tutti::graph::capture` replaced the graph downcasts that used to answer
//! "what is this entity's MIDI port" and "what are its modulatable params".
//! These pin the replacement at the insertion paths themselves —
//! `spawn_audio_node`, `insert_audio_node`, `crossfade_audio_node` — rather
//! than at a fixture that captures by hand, because a path that forgot to
//! capture would leave every hand-captured suite green.

#![cfg(any(all(feature = "midi", feature = "synth"), feature = "modulation"))]

#[macro_use]
mod common;

/// MIDI: the captured `MidiTarget` follows the unit through every insertion.
#[cfg(all(feature = "midi", feature = "synth"))]
mod midi {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;
    use bevy_ecs::system::RunSystemOnce;

    use bevy_tutti::graph::{
        crossfade_audio_node, AudioGraphRes, GraphReconcilePlugin, InsertAudioNode, SpawnAudioNode,
    };
    use bevy_tutti::midi::{MidiTarget, MidiTargetRegistry, MidiTargetResolver};
    use bevy_tutti::AudioEngineState;
    use tutti_core::AudioNode;
    use tutti_midi_types::MidiUnitId;
    use tutti_polysynth::{PolySynth, SynthConfig};

    fn app() -> App {
        let mut app = App::new();
        // Headless: every insertion path dirties the graph, and the commit
        // lands on its own audio side.
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(GraphReconcilePlugin);
        app.init_resource::<MidiTargetRegistry>();
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<PolySynth>();
        app
    }

    fn synth() -> (PolySynth, MidiUnitId) {
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let id = synth.midi_port().unit_id();
        (synth, id)
    }

    /// What resolution answers for `entity` — the port id, if any.
    fn resolved(app: &mut App, entity: Entity) -> Option<MidiUnitId> {
        app.world_mut()
            .run_system_once(move |resolver: MidiTargetResolver| {
                resolver.port(entity).map(|p| p.unit_id())
            })
            .unwrap()
    }

    fn node_of(app: &App, entity: Entity) -> tutti_core::dsp::NodeId {
        app.world().get::<AudioNode>(entity).expect("bound").0
    }

    /// Mutation: making `add_and_bind` bind `CapturedControls::default()`
    /// instead of the captured controls fails this — the entity gets its node
    /// and no port.
    #[test]
    fn spawn_audio_node_captures_a_registered_units_port() {
        let mut app = app();
        let (unit, id) = synth();
        let entity = app.world_mut().commands().spawn_audio_node(unit).id();
        app.update();

        let target = app.world().get::<MidiTarget>(entity).expect("captured");
        assert_eq!(target.port().unit_id(), id, "the synth's own port");
        assert_eq!(target.node(), node_of(&app, entity), "for its own node");
        assert_eq!(resolved(&mut app, entity), Some(id));
    }

    /// The same capture on the adopt-an-entity path.
    ///
    /// Mutation: as above — both paths share `add_and_bind`, and this pins that
    /// `insert_audio_node` still goes through it.
    #[test]
    fn insert_audio_node_captures_a_registered_units_port() {
        let mut app = app();
        let (unit, id) = synth();
        let entity = app.world_mut().spawn_empty().id();
        app.world_mut()
            .commands()
            .entity(entity)
            .insert_audio_node(unit);
        app.update();

        assert_eq!(resolved(&mut app, entity), Some(id));
    }

    /// A crossfade replaces the unit under a surviving `NodeId`, so it must
    /// replace the captured port too — the staleness that used to be the
    /// argument against storing one.
    ///
    /// Mutation: deleting the `controls.replace(..)` call in
    /// `crossfade_audio_node` leaves the first synth's id resolving, and fails
    /// the second assertion.
    #[test]
    fn a_crossfade_recaptures_from_the_incoming_unit() {
        let mut app = app();
        let (first, first_id) = synth();
        let entity = app.world_mut().commands().spawn_audio_node(first).id();
        app.update();
        let node = node_of(&app, entity);

        let (second, second_id) = synth();
        assert_ne!(first_id, second_id, "each port mints its own id");
        crossfade_audio_node(&mut app.world_mut().commands(), entity, Box::new(second));
        app.update();

        assert_eq!(node_of(&app, entity), node, "a crossfade keeps the NodeId");
        assert_eq!(
            resolved(&mut app, entity),
            Some(second_id),
            "and resolves to the incoming unit's port, not the outgoing one's"
        );
    }

    /// Crossfading to a unit with no port removes the target rather than leaving
    /// the old one reachable under the new unit's node.
    ///
    /// Mutation: making `CapturedControls::replace` skip the removal when a
    /// control is absent leaves the synth's port resolving, and fails this.
    #[test]
    fn a_crossfade_to_a_portless_unit_drops_the_target() {
        let mut app = app();
        let (unit, _) = synth();
        let entity = app.world_mut().commands().spawn_audio_node(unit).id();
        app.update();

        crossfade_audio_node(
            &mut app.world_mut().commands(),
            entity,
            Box::new(tutti_nodes::testing::Const::new(
                0.0,
                tutti_types::ChannelLayout::STEREO,
            )),
        );
        app.update();

        assert!(app.world().get::<MidiTarget>(entity).is_none());
        assert_eq!(resolved(&mut app, entity), None);
    }

    /// Taking the node away takes the captured port with it.
    ///
    /// Mutation: dropping the `drop_captured` call from
    /// `reconcile_node_despawn` leaves the `MidiTarget` on the entity.
    #[test]
    fn removing_the_node_drops_the_captured_target() {
        let mut app = app();
        let (unit, _) = synth();
        let entity = app.world_mut().commands().spawn_audio_node(unit).id();
        app.update();
        assert!(app.world().get::<MidiTarget>(entity).is_some());

        app.world_mut().entity_mut(entity).remove::<AudioNode>();
        app.update();
        assert!(app.world().get::<MidiTarget>(entity).is_none());
    }

    /// An `AudioNode` replaced by hand, with no fresh capture, leaves the old
    /// target behind; resolution must not hand it out for the new node.
    ///
    /// Mutation: dropping the `target.node == node.0` check in
    /// `MidiTargetResolver::port` resolves the leftover port and fails this.
    #[test]
    fn a_target_captured_for_another_node_resolves_to_nothing() {
        let mut app = app();
        let (unit, id) = synth();
        let entity = app.world_mut().commands().spawn_audio_node(unit).id();
        app.update();
        assert_eq!(resolved(&mut app, entity), Some(id));

        let other = app.world_mut().resource_mut::<AudioGraphRes>().insert(
            tutti_nodes::testing::Const::new(0.0, tutti_types::ChannelLayout::STEREO),
        );
        app.world_mut().entity_mut(entity).insert(other);

        assert!(
            app.world().get::<MidiTarget>(entity).is_some(),
            "the leftover is still there — this is the case under test"
        );
        assert_eq!(resolved(&mut app, entity), None);
    }
}

/// Modulation: the captured `ModParamsHandle` is what a route binds through.
#[cfg(feature = "modulation")]
mod modulation {
    use bevy_app::prelude::*;
    use bevy_ecs::system::RunSystemOnce;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, SpawnAudioNode, TransportRes};
    use bevy_tutti::modulation::{
        LfoShape, ModParamRange, ModParamsHandle, ModRoute, ModSource, ModSourceRate,
        ModTargetRegistry, ModTargetResolver, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::AudioNode;
    use tutti_nodes::{DistortionNode, ShapeKind};
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    const BASE: f32 = 5.0;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 1));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();
        app
    }

    fn drive_range() -> ModParamRange {
        ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), BASE, 0.0, 10.0)
    }

    /// A route onto a node spawned the ordinary way moves the node's own atomic.
    ///
    /// Mutation: making `CapturedControls::from_registries` capture no params
    /// (`params: None`) leaves the drive at its base — the route is well-formed
    /// and binds to nothing.
    #[test]
    fn a_route_binds_through_the_params_captured_at_spawn() {
        let mut app = app();
        let unit = DistortionNode::new(ShapeKind::Tanh, BASE);
        let drive = unit.drive();
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(unit)
            .insert(drive_range())
            .id();
        // A square at zero rate holds a constant offset.
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );
        app.update();
        app.update();

        assert!(app.world().get::<ModParamsHandle>(target).is_some());
        let now = drive.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            (now - BASE).abs() > 0.1,
            "the route must reach the node's atomic through the captured params; \
             drive is still {now}"
        );
    }

    /// Taking the node away drops the handle — and with it the clone of the
    /// unit it holds, which for a convolver or a synth is not small.
    ///
    /// Mutation: dropping the `drop_captured` call from
    /// `reconcile_node_despawn` leaves the handle on the entity.
    #[test]
    fn removing_the_node_drops_the_captured_handle() {
        let mut app = app();
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(DistortionNode::new(ShapeKind::Tanh, BASE))
            .id();
        app.update();
        assert!(app.world().get::<ModParamsHandle>(target).is_some());

        app.world_mut().entity_mut(target).remove::<AudioNode>();
        app.update();
        assert!(app.world().get::<ModParamsHandle>(target).is_none());
    }

    /// A handle captured for another node is ignored.
    ///
    /// Mutation: dropping the `handle.node != node.0` check in
    /// `ModTargetResolver::resolve` resolves through the leftover handle and
    /// fails this.
    #[test]
    fn a_handle_captured_for_another_node_resolves_to_nothing() {
        let mut app = app();
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(DistortionNode::new(ShapeKind::Tanh, BASE))
            .id();
        app.update();

        let range = drive_range().params[0];
        let resolves = |app: &mut App| {
            app.world_mut()
                .run_system_once(move |resolver: ModTargetResolver| {
                    resolver.resolve(target, range.param, &range).is_some()
                })
                .unwrap()
        };
        assert!(resolves(&mut app), "the fresh capture resolves");

        let other = app
            .world_mut()
            .resource_mut::<AudioGraphRes>()
            .insert(tutti_nodes::testing::Const::mono(0.0));
        app.world_mut().entity_mut(target).insert(other);
        assert!(!resolves(&mut app), "the leftover handle does not");
    }
}

/// After a crossfade, everything that reads a captured control reaches the
/// **incoming** unit: its MIDI route, its modulation, and its sequence.
///
/// Each consumer compiles what it read into something longer-lived — a route
/// table naming a port id, an accumulator mirroring into an atomic, a clip
/// installed on a port — and rebuilds only when its inputs change. A crossfade
/// changes none of the declarations, only the captured controls, so each has to
/// treat a changed capture as a reason to rebuild.
///
/// # Mutation
///
/// Each arm is pinned by its own assertion:
///
/// - dropping `recaptured` from the MIDI route `rebuild`'s dirty check leaves
///   the route naming the outgoing port;
/// - dropping `Changed<ModParamsHandle>` from the modulation
///   `mark_dirty_on_route_change` leaves the accumulator on the outgoing unit's
///   volume atomic (the incoming one reads its untouched value);
/// - dropping `recaptured` from the sequence `rebuild` leaves the clip on the
///   outgoing port, so the incoming one polls no note.
#[cfg(all(feature = "midi", feature = "synth", feature = "modulation"))]
mod crossfade_consumers {
    use bevy_app::prelude::*;

    use bevy_tutti::graph::{
        crossfade_audio_node, AudioConfig, AudioGraphRes, GraphReconcilePlugin, SpawnAudioNode,
        TransportRes,
    };
    use bevy_tutti::midi::{
        MidiRouteRule, MidiSourceInstall, MidiTarget, MidiTargetRegistry, TuttiMidiPlugin,
    };
    use bevy_tutti::modulation::{
        LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
        TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::{Beat, SampleRate};
    use tutti_midi_runtime::TimedMidiEvent;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup, MidiUnitId};
    use tutti_polysynth::{PolySynth, SynthConfig};
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    const SAMPLE_RATE: f64 = 48_000.0;
    const BASE_VOLUME: f32 = 0.5;

    fn synth() -> PolySynth {
        PolySynth::new(SynthConfig::default()).expect("builds a synth")
    }

    #[test]
    fn a_crossfaded_synth_keeps_its_route_its_modulation_and_its_sequence() {
        let mut graph = AudioGraphRes::headless(0, 2);
        graph.set_sample_rate(SampleRate(SAMPLE_RATE));
        let _backend = graph.take_audio_side();

        let mut app = App::new();
        app.insert_resource(graph);
        app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
        app.insert_resource(AudioConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
            SAMPLE_RATE,
        ));
        let (routing, rt_view) = bevy_tutti::midi::test_support::routing_table_for_test();
        app.insert_resource(routing);
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<PolySynth>();
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<PolySynth>();

        // One synth, with all three consumers pointed at it.
        let outgoing = synth();
        let outgoing_volume = outgoing.volume_atomic();
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(outgoing)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Volume),
                BASE_VOLUME,
                0.0,
                1.0,
            ))
            .id();
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(target));
        // A square at zero rate holds a constant offset.
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Volume)).with_depth(Depth(0.2)),
        );
        let on = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, u16::MAX);
        app.world_mut().spawn(MidiSourceInstall::new(
            target,
            vec![TimedMidiEvent::new(Beat(0.25), on)],
        ));
        for _ in 0..3 {
            app.update();
        }
        // Every consumer is bound to the outgoing unit before the crossfade, so
        // what follows is a *re*-binding and not a first binding that happened
        // to land late.
        let driven = outgoing_volume.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            (driven - BASE_VOLUME).abs() > 0.05,
            "precondition: the LFO drives the outgoing unit"
        );

        // The incoming unit, with a handle on its own volume atomic.
        let incoming = synth();
        let volume = incoming.volume_atomic();
        let untouched = volume.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            (untouched - driven).abs() > 0.05,
            "precondition: the incoming unit's own volume ({untouched}) is not already \
             the driven value ({driven}), or the assertion below proves nothing"
        );
        crossfade_audio_node(&mut app.world_mut().commands(), target, Box::new(incoming));
        app.update();
        app.update();
        let port = app
            .world()
            .get::<MidiTarget>(target)
            .unwrap()
            .port()
            .clone();
        let new_id: MidiUnitId = port.unit_id();

        // MIDI route: the table names the incoming port.
        let routed: Vec<MidiUnitId> = rt_view.read().route(&on).collect();

        // Modulation: the accumulator mirrors into the incoming unit's atomic.
        let modulated = volume.load(std::sync::atomic::Ordering::Acquire);

        // Sequence: the clip plays out of the incoming port.
        let transport = app.world().resource::<TransportRes>().clone();
        let _ = transport
            .motion
            .try_send(tutti_core::transport::MotionEvent::Play);
        transport.motion.drain();
        let mut buf = [MidiEvent::noop(); 256];
        let n = port.poll(24_000, SampleRate(SAMPLE_RATE), &mut buf);
        let sequenced = buf[..n].iter().any(|e| e.is_note_on());

        assert_eq!(
            routed,
            vec![new_id],
            "the MIDI route names the incoming port"
        );
        assert!(
            (modulated - driven).abs() < 1e-3,
            "the LFO must drive the incoming unit's volume to {driven}; it reads \
             {modulated} (untouched: {untouched})"
        );
        assert!(sequenced, "the sequence must play out of the incoming port");
    }
}

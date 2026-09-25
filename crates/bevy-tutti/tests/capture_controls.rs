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
    use tutti_core::dsp::Net;
    use tutti_core::AudioNode;
    use tutti_midi_types::MidiUnitId;
    use tutti_polysynth::{PolySynth, SynthConfig};

    fn app() -> App {
        let mut app = App::new();
        // A backend: every insertion path dirties the graph, and the commit
        // asserts one exists.
        app.insert_resource(AudioGraphRes(Net::with_backend(2)));
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

        let other = app.world_mut().resource_mut::<AudioGraphRes>().0.add(
            tutti_nodes::testing::Const::new(0.0, tutti_types::ChannelLayout::STEREO),
        );
        app.world_mut().entity_mut(entity).insert(AudioNode(other));

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
    use tutti_core::dsp::Net;
    use tutti_core::transport::Transport;
    use tutti_core::AudioNode;
    use tutti_nodes::{DistortionNode, ShapeKind};
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    const BASE: f32 = 5.0;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(Net::with_backend(1)));
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
            .0
            .add(tutti_nodes::testing::Const::mono(0.0));
        app.world_mut().entity_mut(target).insert(AudioNode(other));
        assert!(!resolves(&mut app), "the leftover handle does not");
    }
}

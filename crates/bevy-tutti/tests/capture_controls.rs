//! Every insertion path captures a unit's controls before the unit enters the
//! graph, and the readers ignore a capture that belongs to another node.
//!
//! `bevy_tutti::graph::capture` replaced the graph downcasts that used to answer
//! "what are this entity's modulatable params".
//! These pin the replacement at the insertion paths themselves —
//! `spawn_audio_node`, `insert_audio_node`, `crossfade_audio_node` — rather
//! than at a fixture that captures by hand, because a path that forgot to
//! capture would leave every hand-captured suite green.

#![cfg(feature = "modulation")]

#[macro_use]
mod common;

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

/// After a crossfade, the modulation reaches the **incoming** unit.
///
/// The modulation compiles what it read into something longer-lived (an
/// accumulator mirroring into an atomic) and rebuilds only when its inputs
/// change. A crossfade changes none of the declarations, only the captured
/// controls, so it has to treat a changed capture as a reason to rebuild.
/// (MIDI reaches a node over event edges keyed by entity, which the event
/// wiring re-derives when the node changes: `midi_sequence.rs` pins that.)
///
/// # Mutation
///
/// Dropping `Changed<ModParamsHandle>` from the modulation
/// `mark_dirty_on_route_change` leaves the accumulator on the outgoing unit's
/// volume atomic (the incoming one reads its untouched value).
#[cfg(all(feature = "synth", feature = "modulation"))]
mod crossfade_consumers {
    use bevy_app::prelude::*;

    use bevy_tutti::graph::{
        crossfade_audio_node, AudioConfig, AudioGraphRes, GraphReconcilePlugin, SpawnAudioNode,
        TransportRes,
    };
    use bevy_tutti::modulation::{
        LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
        TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::SampleRate;
    use tutti_polysynth::{PolySynth, SynthConfig};
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    const SAMPLE_RATE: f64 = 48_000.0;
    const BASE_VOLUME: f32 = 0.5;

    fn synth() -> PolySynth {
        PolySynth::new(SynthConfig::default()).expect("builds a synth")
    }

    #[test]
    fn a_crossfaded_synth_keeps_its_modulation() {
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
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<PolySynth>();

        // One synth, modulated.
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
        for _ in 0..3 {
            app.update();
        }
        // The modulation is bound to the outgoing unit before the crossfade, so
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
        // The accumulator mirrors into the incoming unit's atomic.
        let modulated = volume.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            (modulated - driven).abs() < 1e-3,
            "the LFO must drive the incoming unit's volume to {driven}; it reads \
             {modulated} (untouched: {untouched})"
        );
    }
}

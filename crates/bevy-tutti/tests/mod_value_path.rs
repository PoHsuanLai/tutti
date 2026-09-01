//! Frame-rate ("value path") modulation: the adapter driven end to end.
//!
//! Everything here delivers a route as a scalar written once per frame, as
//! opposed to the per-sample graph chain in `mod_audio_rate.rs`. One module per
//! scenario:
//!
//! - `modulation` — the adapter itself: a real `App`, a real graph, real params.
//! - `mod_cascade` — one LFO modulating another LFO's rate, declared in the ECS.
//! - `mod_curve_delivery` — a route delivered as a beat-evaluated curve.
//! - `mod_source` — a modulator kind the adapter has never heard of.
//!
//! Each was its own file; they share the value path as their subject and are
//! grouped by it. Bodies and test names are unchanged from those files, and each
//! module keeps its own helpers so nothing is coupled across the seam.

#![cfg(feature = "modulation")]

/// The modulation adapter driven end-to-end: a real `App`, a real graph, a real
/// node, and the node's own atomic checked for movement.
///
/// The declaration → matrix → node-atomic path is the whole point of the layer,
/// and it is the part a unit test of any single piece would miss. Every test
/// here asserts on the value the DSP actually reads.
/// (Was `tests/modulation.rs`.)
mod modulation {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::modulation::{
        LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
        ModulationMatrix, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::transport::Transport;
    use tutti_core::AudioNode;
    use tutti_nodes::DistortionNode;
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    /// The drive an unmodulated node holds — its constructor argument, and what the
    /// atomic must still read when nothing routes to it.
    const UNMODULATED_DRIVE: f32 = 1.0;

    /// A `Drive`-modulatable node whose param atomic we can read back.
    fn drive_node() -> DistortionNode {
        DistortionNode::new(tutti_nodes::ShapeKind::Tanh, UNMODULATED_DRIVE)
    }

    /// An app with the reconcile pipeline, a live graph, and modulation — the same
    /// wiring a host gets, minus the audio device.
    fn app_with_graph() -> (App, Entity) {
        let mut app = App::new();

        let mut net = Net::new(0, 1);
        let node = net.push(Box::new(drive_node()));
        net.pipe_output(node);

        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        // The systems are gated on a running engine; nothing here opens a device,
        // so the state stands in for one.
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));

        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        let target = app.world_mut().spawn(AudioNode(node)).id();
        (app, target)
    }

    /// The node's live drive value — what the DSP reads, not what the matrix thinks.
    fn node_drive(app: &App, entity: Entity) -> f32 {
        let node = app.world().get::<AudioNode>(entity).unwrap().0;
        let graph = app.world().resource::<AudioGraphRes>();
        graph
            .0
            .node_as::<DistortionNode>(node)
            .unwrap()
            .drive()
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn declare_drive_range(app: &mut App, target: Entity, base: f32) {
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                base,
                0.0,
                10.0,
            ));
    }

    /// Advance the transport by `samples`, as the audio clock would.
    fn advance_transport(app: &mut App, samples: i64) {
        let transport = app.world().resource::<TransportRes>().clone();
        let current = transport.settings.steady_time();
        transport
            .settings
            .steady_time
            .store(current + samples, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn a_route_moves_the_target_nodes_own_atomic() {
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);

        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth::FULL),
        );

        app.update();
        assert_eq!(
            app.world().resource::<ModulationMatrix>().len(),
            1,
            "the route should have resolved to one target"
        );

        // A sine starts at zero, so the first frame sits at base; run far enough
        // into the cycle for it to have swung.
        let mut moved = false;
        for _ in 0..30 {
            advance_transport(&mut app, 480); // 10ms at 48k
            app.update();
            if (node_drive(&app, target) - 5.0).abs() > 0.1 {
                moved = true;
            }
        }
        assert!(moved, "the LFO should have moved the node's drive atomic");
    }

    #[test]
    fn an_unregistered_node_type_resolves_to_nothing() {
        // The registry is what makes resolution possible; without the node type
        // registered a route is inert rather than panicking.
        let mut app = App::new();
        let mut net = Net::new(0, 1);
        let node = net.push(Box::new(drive_node()));
        net.pipe_output(node);
        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        // Deliberately no `.register::<DistortionNode>()`.

        let target = app.world_mut().spawn(AudioNode(node)).id();
        declare_drive_range(&mut app, target, 5.0);
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id();
        app.world_mut().spawn(ModRoute::new(
            lfo,
            target,
            ParamAddr::Unit(UnitParam::Drive),
        ));

        app.update();

        assert!(app.world().resource::<ModulationMatrix>().is_empty());
        assert_eq!(
            node_drive(&app, target),
            UNMODULATED_DRIVE,
            "the node keeps its own value"
        );
    }

    #[test]
    fn a_param_without_a_declared_range_is_not_modulated() {
        // `ModParamRange` is how a host says "this is modulatable, over this
        // range". Without it there is no base or clamp to accumulate against.
        let (mut app, target) = app_with_graph();
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id();
        app.world_mut().spawn(ModRoute::new(
            lfo,
            target,
            ParamAddr::Unit(UnitParam::Drive),
        ));

        app.update();

        assert!(app.world().resource::<ModulationMatrix>().is_empty());
    }

    #[test]
    fn the_claim_set_reports_which_params_are_modulated() {
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id();
        app.world_mut().spawn(ModRoute::new(
            lfo,
            target,
            ParamAddr::Unit(UnitParam::Drive),
        ));

        app.update();

        let matrix = app.world().resource::<ModulationMatrix>();
        assert!(matrix.is_modulated(target, ParamAddr::Unit(UnitParam::Drive)));
        // A param nobody routed to is the reconciler's to write.
        assert!(!matrix.is_modulated(target, ParamAddr::Unit(UnitParam::Cutoff)));
    }

    // The two `set_base` tests moved into `modulation/driver.rs` when the method
    // became `pub(crate)` — an integration test cannot reach it. They still build a
    // real `App` and assert on the node's atomic; only their address changed. The
    // public path they used to stand in for is covered by
    // `an_authored_write_to_a_modulated_param_moves_the_base` in `audio_param.rs`.

    #[test]
    fn removing_a_route_returns_the_param_to_its_base() {
        // The continuous-value tax: modulation offsets are never "released", so a
        // deleted edge would leave its last offset stuck on the param forever if
        // the stale-layer sweep did not clear it.
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        let route = app
            .world_mut()
            .spawn(
                ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.2)),
            )
            .id();

        app.update();
        advance_transport(&mut app, 480);
        app.update();
        let modulated = node_drive(&app, target);
        assert!(
            (modulated - 5.0).abs() > 0.1,
            "square at full depth should hold the param off its base"
        );

        app.world_mut().entity_mut(route).despawn();
        advance_transport(&mut app, 480);
        app.update();

        assert!(
            (node_drive(&app, target) - 5.0).abs() < 1e-3,
            "the removed route's layer must be cleared, not left stuck"
        );
        assert!(app.world().resource::<ModulationMatrix>().is_empty());
    }

    #[test]
    fn two_routes_onto_one_param_sum_instead_of_overwriting() {
        // Distinct layer keys per route are what makes this hold: with a shared key
        // the second route would overwrite the first's contribution in place.
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);

        let mut spawn_square = || {
            app.world_mut()
                .spawn((
                    ModSource::new(LfoShape::Square),
                    ModSourceRate::free_running(Hz(0.0)),
                ))
                .id()
        };
        let (a, b) = (spawn_square(), spawn_square());

        for source in [a, b] {
            app.world_mut().spawn(
                ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.1)),
            );
        }

        app.update();
        advance_transport(&mut app, 480);
        app.update();
        let two = node_drive(&app, target);

        // One route alone, for comparison.
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);
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
        advance_transport(&mut app, 480);
        app.update();
        let one = node_drive(&app, target);

        assert!(
            (two - 5.0).abs() > (one - 5.0).abs() * 1.5,
            "two routes should push further than one: one={one}, two={two}"
        );
    }

    #[test]
    fn a_disabled_route_contributes_nothing() {
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        let mut route =
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2));
        route.enabled = false;
        app.world_mut().spawn(route);

        app.update();
        advance_transport(&mut app, 480);
        app.update();

        assert!(app.world().resource::<ModulationMatrix>().is_empty());
        assert_eq!(node_drive(&app, target), UNMODULATED_DRIVE);
    }

    #[test]
    fn a_steady_transport_does_not_rebuild_the_matrix() {
        // Rebuilding mints fresh sources, and a fresh source starts at phase zero.
        // If a quiet frame rebuilt, every LFO would restart 60 times a second and
        // never advance — so the change-gate is load-bearing, not an optimization.
        let (mut app, target) = app_with_graph();
        declare_drive_range(&mut app, target, 5.0);
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id();
        app.world_mut().spawn(ModRoute::new(
            lfo,
            target,
            ParamAddr::Unit(UnitParam::Drive),
        ));

        app.update();
        let first = app
            .world()
            .resource::<ModulationMatrix>()
            .target(target, ParamAddr::Unit(UnitParam::Drive))
            .cloned()
            .expect("resolved");

        for _ in 0..5 {
            advance_transport(&mut app, 480);
            app.update();
        }

        let later = app
            .world()
            .resource::<ModulationMatrix>()
            .target(target, ParamAddr::Unit(UnitParam::Drive))
            .cloned()
            .expect("still resolved");

        assert!(
            std::sync::Arc::ptr_eq(&first, &later),
            "a quiet frame must not rebuild the matrix"
        );
    }

    /// Reflection has to reach the *leaves* to be worth anything: an editor showing
    /// a route needs the `Depth` inside it, not just the struct's name. Walking down
    /// to the float is what distinguishes real reflection from a derive that
    /// compiles.
    #[test]
    fn a_route_reflects_down_to_its_depth() {
        use bevy_reflect::{PartialReflect, ReflectRef};

        // Real entities rather than synthesized ids: nothing here dereferences
        // them, but spawning keeps the test off `Entity`'s construction API.
        let mut world = World::new();
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let route = ModRoute::new(a, b, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.25));

        let ReflectRef::Struct(s) = route.reflect_ref() else {
            panic!("ModRoute should reflect as a struct");
        };
        let depth = s.field("depth").expect("a `depth` field");

        // A unit newtype is a tuple struct, so it reflects with an indexed field —
        // and reflecting *through* to that float is the whole point: an opaque
        // value would stop the walk here.
        let ReflectRef::TupleStruct(depth) = depth.reflect_ref() else {
            panic!("Depth should reflect as a tuple struct, not an opaque value");
        };
        let inner = depth
            .field(0)
            .expect("Depth's inner float")
            .try_downcast_ref::<f32>()
            .expect("f32");
        assert_eq!(*inner, 0.25);
    }

    /// The types are registered, so a scene or an inspector can find them by name
    /// rather than only through a value that already exists.
    #[test]
    fn the_components_are_registered_for_reflection() {
        use bevy_ecs::reflect::AppTypeRegistry;

        let (app, _) = app_with_graph();
        let registry = app.world().resource::<AppTypeRegistry>().read();

        for name in [
            std::any::type_name::<ModSource>(),
            std::any::type_name::<ModSourceRate>(),
            std::any::type_name::<ModRoute>(),
            std::any::type_name::<ModParamRange>(),
        ] {
            assert!(
                registry.get_with_type_path(name).is_some(),
                "{name} should be registered"
            );
        }
    }
}

/// One LFO modulating another LFO's rate, declared entirely in the ECS.
///
/// A modulation source is not a graph node, so the ordinary resolver path —
/// `AudioNode` then downcast — could never serve a route onto a source's own
/// rate. These tests cover the second path: a source carries a live rate cell,
/// and the accumulator that drives it mirrors into that same cell.
///
/// Sharing exactly one cell is the load-bearing part, and it is the part that
/// fails *silently* — a second cell type-checks, runs, and modulates nothing —
/// so the assertions below read the value the downstream source actually runs
/// at rather than any bookkeeping about it.
/// (Was `tests/mod_cascade.rs`.)
mod mod_cascade {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::modulation::{
        LfoShape, ModParamRange, ModRateCell, ModRoute, ModSource, ModSourceRate,
        ModTargetRegistry, ModulationMatrix, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::transport::Transport;
    use tutti_core::AudioNode;
    use tutti_nodes::DistortionNode;
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    /// The rate the modulated LFO is authored at, and the floor of its range.
    const CARRIER_RATE: f32 = 2.0;

    fn app_with_graph() -> (App, Entity) {
        let mut app = App::new();

        let mut net = Net::new(0, 1);
        let node = net.push(Box::new(DistortionNode::new(
            tutti_nodes::ShapeKind::Tanh,
            1.0,
        )));
        net.pipe_output(node);

        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        let target = app.world_mut().spawn(AudioNode(node)).id();
        (app, target)
    }

    fn advance_transport(app: &mut App, samples: i64) {
        let transport = app.world().resource::<TransportRes>().clone();
        let current = transport.settings.steady_time();
        transport
            .settings
            .steady_time
            .store(current + samples, std::sync::atomic::Ordering::Relaxed);
    }

    /// Spawn a source whose rate is driven by another source, and return both.
    ///
    /// `carrier` is the LFO whose rate moves; `modulator` drives it. The carrier
    /// declares `Rate` modulatable over `[2, 10]` Hz — the range is the *host's* to
    /// declare here exactly as it is for a node's param.
    fn spawn_cascade(app: &mut App) -> (Entity, Entity) {
        let carrier = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(CARRIER_RATE)),
                ModParamRange::default().with(
                    ParamAddr::Unit(UnitParam::Rate),
                    CARRIER_RATE,
                    CARRIER_RATE,
                    10.0,
                ),
            ))
            .id();

        let modulator = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(1.0)),
            ))
            .id();

        app.world_mut().spawn(
            ModRoute::new(modulator, carrier, ParamAddr::Unit(UnitParam::Rate))
                .with_depth(Depth::FULL),
        );

        (carrier, modulator)
    }

    /// The cell exists only where a route asks for one — it is not spawned onto
    /// every source just in case.
    #[test]
    fn only_a_routed_source_gets_a_rate_cell() {
        let (mut app, _) = app_with_graph();
        let (carrier, modulator) = spawn_cascade(&mut app);
        app.update();

        assert!(
            app.world().get::<ModRateCell>(carrier).is_some(),
            "the routed-to source should have been given a rate cell"
        );
        assert!(
            app.world().get::<ModRateCell>(modulator).is_none(),
            "a source nothing routes at should not carry one"
        );
    }

    /// The route resolves against a source entity, which carries no `AudioNode` at
    /// all — so this can only have gone through the rate path.
    #[test]
    fn a_route_onto_a_sources_rate_resolves() {
        let (mut app, _) = app_with_graph();
        let (carrier, _) = spawn_cascade(&mut app);
        app.update();

        assert!(
            app.world().get::<AudioNode>(carrier).is_none(),
            "a modulation source is not a graph node — the node path cannot serve it"
        );
        assert!(
            app.world()
                .resource::<ModulationMatrix>()
                .is_modulated(carrier, ParamAddr::Unit(UnitParam::Rate)),
            "the carrier's rate should be claimed by the matrix"
        );
    }

    /// The end-to-end claim: the driver actually moves the cell the carrier reads.
    ///
    /// Asserting on the cell rather than on some downstream audible effect keeps
    /// the failure legible — if this moves, the cascade is wired; if it does not,
    /// the two halves are looking at different cells.
    #[test]
    fn the_modulator_moves_the_carriers_live_rate() {
        let (mut app, _) = app_with_graph();
        let (carrier, _) = spawn_cascade(&mut app);
        app.update();

        let seeded = app
            .world()
            .get::<ModRateCell>(carrier)
            .expect("carrier has a cell")
            .frequency();
        assert!(
            (seeded.get() - CARRIER_RATE).abs() < 1e-5,
            "the cell should start at the authored rate, got {seeded:?}"
        );

        // Sweep the modulating sine; its positive half must push the carrier's rate
        // above the base it was seeded with.
        let mut peak = seeded.get();
        for _ in 0..40 {
            advance_transport(&mut app, 480);
            app.update();
            let live = app
                .world()
                .get::<ModRateCell>(carrier)
                .expect("carrier keeps its cell")
                .frequency();
            peak = peak.max(live.get());
        }

        assert!(
            peak > CARRIER_RATE + 0.1,
            "the modulator should have driven the carrier's rate above {CARRIER_RATE}, peaked at {peak}"
        );
    }

    /// The cascade must reach the *downstream source's phase*, not merely the cell.
    ///
    /// This is the assertion that catches the failure mode the whole design exists
    /// to prevent: build the `Sourced` reading a cell other than the one the
    /// accumulator writes, and every cell-level check above still passes — the
    /// accumulator moves its cell correctly, it just isn't the cell anyone reads.
    /// Only the carrier's own output can tell the difference.
    ///
    /// So the carrier is pointed at a node's `Drive` and its waveform sampled. A
    /// carrier whose rate is being driven sweeps at a different speed than one at a
    /// fixed 2 Hz, so the two visit measurably different value sets.
    #[test]
    fn a_driven_rate_changes_the_carriers_own_output() {
        /// Sample the drive a `carrier`-driven node reads over a fixed window.
        ///
        /// `cascaded` decides whether the carrier's rate is itself modulated; both
        /// arms are otherwise identical, so any divergence is the cascade.
        fn drive_trace(cascaded: bool) -> Vec<f32> {
            let (mut app, target) = app_with_graph();

            let carrier = app
                .world_mut()
                .spawn((
                    ModSource::new(LfoShape::Sine),
                    ModSourceRate::free_running(Hz(CARRIER_RATE)),
                    ModParamRange::default().with(
                        ParamAddr::Unit(UnitParam::Rate),
                        CARRIER_RATE,
                        CARRIER_RATE,
                        10.0,
                    ),
                ))
                .id();

            // The carrier drives a node param, so its phase is observable.
            app.world_mut()
                .entity_mut(target)
                .insert(ModParamRange::default().with(
                    ParamAddr::Unit(UnitParam::Drive),
                    5.0,
                    0.0,
                    10.0,
                ));
            app.world_mut().spawn(
                ModRoute::new(carrier, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.4)),
            );

            if cascaded {
                let modulator = app
                    .world_mut()
                    .spawn((
                        ModSource::new(LfoShape::Sine),
                        ModSourceRate::free_running(Hz(1.0)),
                    ))
                    .id();
                app.world_mut().spawn(
                    ModRoute::new(modulator, carrier, ParamAddr::Unit(UnitParam::Rate))
                        .with_depth(Depth::FULL),
                );
            }

            let mut trace = Vec::new();
            for _ in 0..60 {
                advance_transport(&mut app, 480);
                app.update();
                let node = app.world().get::<AudioNode>(target).unwrap().0;
                trace.push(
                    app.world()
                        .resource::<AudioGraphRes>()
                        .0
                        .node_as::<DistortionNode>(node)
                        .unwrap()
                        .drive()
                        .load(std::sync::atomic::Ordering::Acquire),
                );
            }
            trace
        }

        let plain = drive_trace(false);
        let cascaded = drive_trace(true);

        // Both must actually be moving, or "they differ" would be vacuous.
        let spread = |t: &[f32]| {
            t.iter().copied().fold(f32::MIN, f32::max) - t.iter().copied().fold(f32::MAX, f32::min)
        };
        assert!(
            spread(&plain) > 0.1 && spread(&cascaded) > 0.1,
            "both carriers should be sweeping: {} vs {}",
            spread(&plain),
            spread(&cascaded)
        );

        let diverged = plain
            .iter()
            .zip(&cascaded)
            .filter(|(a, b)| (*a - *b).abs() > 1e-3)
            .count();
        assert!(
            diverged > 5,
            "a driven rate must change when the carrier reaches each phase — \
             only {diverged}/60 samples differed, so the carrier is still running \
             at its fixed rate and is reading a cell nobody drives"
        );
    }

    /// A cascade must survive a rebuild. `collect` reconstructs every `Sourced` when
    /// the declaration changes, so a cell minted during the build would be replaced
    /// and the accumulator left writing an orphan — the exact bug the component
    /// exists to prevent, and one that only shows up on the *second* build.
    #[test]
    fn the_cascade_survives_a_rebuild() {
        let (mut app, target) = app_with_graph();
        let (carrier, _) = spawn_cascade(&mut app);
        app.update();

        let before = app
            .world()
            .get::<ModRateCell>(carrier)
            .expect("carrier has a cell")
            .as_atomic();

        // Force a rebuild by declaring an unrelated route — enough to make the
        // whole source registry be rebuilt from scratch.
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                5.0,
                0.0,
                10.0,
            ));
        let other = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Triangle),
                ModSourceRate::free_running(Hz(3.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(other, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
        );
        app.update();

        let after = app
            .world()
            .get::<ModRateCell>(carrier)
            .expect("carrier keeps its cell")
            .as_atomic();
        assert!(
            std::sync::Arc::ptr_eq(&before, &after),
            "the rate cell must be the same allocation across a rebuild"
        );

        // And it must still be driven after that rebuild.
        let mut peak = 0.0_f32;
        for _ in 0..40 {
            advance_transport(&mut app, 480);
            app.update();
            peak = peak.max(
                app.world()
                    .get::<ModRateCell>(carrier)
                    .expect("carrier keeps its cell")
                    .frequency()
                    .get(),
            );
        }
        assert!(
            peak > CARRIER_RATE + 0.1,
            "the cascade should still drive the rate after a rebuild, peaked at {peak}"
        );
    }
}

/// A route delivered as a beat-evaluated curve rather than a per-frame scalar.
///
/// The sink here is defined *in the test*: a `LayeredCurve` that accepts curve
/// layers, standing in for the kind of accumulator a plugin's per-block
/// parameter producer holds. bevy-tutti ships no such sink — `AtomicTarget`
/// collapses at a fixed beat and declines curves — so this also exercises
/// `ModTargetRegistry::insert_target`, the only route to a sink no `AudioUnit`
/// owns.
///
/// What the delivery mode buys is *when* the value is decided, not what it is:
/// a scalar is computed once per frame and stored, a curve is stored as a
/// function and sampled by the sink at whatever rate it reads.
/// (Was `tests/mod_curve_delivery.rs`.)
mod mod_curve_delivery {
    use std::sync::{Arc, Mutex};

    use bevy_app::prelude::*;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::modulation::{
        LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
        TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::transport::Transport;
    use tutti_mod::{Curve, LayerKey, LayeredCurve, ModTarget};
    use tutti_types::{Beat, BeatDuration, ParamAddr, UnitParam};

    /// A sink that takes curve layers — the shape a sub-block reader has.
    ///
    /// `AtomicTarget` mirrors a frame scalar into an atomic; this keeps the whole
    /// `LayeredCurve` so it can be evaluated at any beat, which is exactly what a
    /// plugin's per-block producer does with `PluginParamTarget`.
    struct BeatSink {
        layered: Mutex<LayeredCurve<f32>>,
    }

    impl BeatSink {
        fn new(base: f32, min: f32, max: f32) -> Self {
            Self {
                layered: Mutex::new(LayeredCurve::new(base, min, max)),
            }
        }

        /// The summed value at `beat` — what a per-block reader would sample.
        fn value_at(&self, beat: Beat) -> f32 {
            self.layered.lock().unwrap().value_at(beat).unwrap_or(0.0)
        }
    }

    impl ModTarget for BeatSink {
        fn range(&self) -> (f32, f32) {
            self.layered.lock().unwrap().range()
        }
        fn base(&self) -> f32 {
            self.layered.lock().unwrap().base()
        }
        fn set_base(&self, value: f32) {
            self.layered.lock().unwrap().set_base(value);
        }
        fn accumulate(&self, key: LayerKey, offset: f32) {
            self.layered.lock().unwrap().set_scalar_layer(key, offset);
        }
        fn clear(&self, key: LayerKey) {
            self.layered.lock().unwrap().clear_layer(key);
        }
        fn final_value(&self) -> f32 {
            self.value_at(Beat(0.0))
        }
        /// The whole point of this sink: it holds curves, so it accepts them.
        fn accumulate_curve(&self, key: LayerKey, curve: Arc<dyn Curve>) -> bool {
            self.layered.lock().unwrap().set_layer(key, curve);
            true
        }
    }

    const BASE: f32 = 5.0;

    fn app() -> App {
        let mut app = App::new();
        let mut net = Net::new(0, 1);
        let out = net.push(Box::new(tutti_nodes::DistortionNode::new(
            tutti_nodes::ShapeKind::Tanh,
            1.0,
        )));
        net.pipe_output(out);
        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app
    }

    /// Wire one source into one host-supplied sink. `as_curve` picks the delivery.
    ///
    /// The param is `Drive` on an entity with no `AudioNode` at all — nothing but
    /// the supplied sink could serve it, so a resolution here proves the supplied
    /// path is what answered.
    fn wire(app: &mut App, as_curve: bool) -> Arc<BeatSink> {
        let sink = Arc::new(BeatSink::new(BASE, 0.0, 10.0));
        let param = ParamAddr::Unit(UnitParam::Drive);

        let target = app
            .world_mut()
            .spawn(ModParamRange::default().with(param, BASE, 0.0, 10.0))
            .id();
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .insert_target(target, param, Arc::clone(&sink) as Arc<dyn ModTarget>);

        // Beat-synced: a curve is clocked by the beat, so only a beat-synced rate
        // has a curve form at all.
        let source = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::beat_synced(BeatDuration(1.0)),
            ))
            .id();

        let route = ModRoute::new(source, target, param);
        app.world_mut()
            .spawn(if as_curve { route.per_block() } else { route });

        app.update();
        sink
    }

    /// A sink no `AudioUnit` owns is reachable at all — the gap `insert_target`
    /// closes. Without it, resolution needs an `AudioNode` and a registered node
    /// type, so this entity could never have been modulated.
    #[test]
    fn a_host_supplied_sink_resolves_without_a_graph_node() {
        let mut app = app();
        let sink = wire(&mut app, false);
        app.update();

        assert!(
            !sink.layered.lock().unwrap().is_unlayered(),
            "the route should have installed a layer on the supplied sink"
        );
    }

    /// The payoff: a curve layer varies *between* frames, so a reader sampling
    /// faster than the frame rate sees motion a scalar cannot give it.
    #[test]
    fn a_curve_delivered_route_varies_within_a_frame() {
        let mut app = app();
        let sink = wire(&mut app, true);
        app.update();

        // Sample across one beat without running a single extra frame — exactly
        // what a per-block producer does inside one callback.
        let traced: Vec<f32> = (0..8)
            .map(|i| sink.value_at(Beat(i as f64 / 8.0)))
            .collect();
        let moved = traced
            .iter()
            .filter(|v| (*v - traced[0]).abs() > 1e-4)
            .count();
        assert!(
            moved >= 6,
            "a curve layer must trace across the beat with no frames run: {traced:?}"
        );
    }

    /// The same route scalar-delivered: one value per frame, frozen between them.
    ///
    /// This is the control for the test above — without it, "the value varied"
    /// could just mean the driver ran.
    #[test]
    fn a_scalar_delivered_route_holds_between_frames() {
        let mut app = app();
        let sink = wire(&mut app, false);
        app.update();

        let traced: Vec<f32> = (0..8)
            .map(|i| sink.value_at(Beat(i as f64 / 8.0)))
            .collect();
        assert!(
            traced.iter().all(|v| (v - traced[0]).abs() < 1e-6),
            "a scalar layer is beat-independent within a frame: {traced:?}"
        );
    }

    /// Asking for a curve on a sink that only takes scalars must still modulate.
    ///
    /// `AtomicTarget` — every native node's accumulator — declines curves, so the
    /// request has to degrade rather than fail. A route that silently stopped
    /// working when its sink said no would be far worse than a coarser one.
    #[test]
    fn a_curve_request_falls_back_when_the_sink_declines() {
        let mut app = app();
        let param = ParamAddr::Unit(UnitParam::Drive);

        // A real graph node, whose `ModParams` hands back an `AtomicTarget`.
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.push(Box::new(tutti_nodes::DistortionNode::new(
                tutti_nodes::ShapeKind::Tanh,
                BASE,
            )))
        };
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<tutti_nodes::DistortionNode>();

        let target = app
            .world_mut()
            .spawn((
                tutti_core::AudioNode(node),
                ModParamRange::default().with(param, BASE, 0.0, 10.0),
            ))
            .id();
        let source = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::beat_synced(BeatDuration(1.0)),
            ))
            .id();
        // Asks for a curve; the atomic sink will decline.
        app.world_mut()
            .spawn(ModRoute::new(source, target, param).per_block());

        let drive = |app: &App| {
            app.world()
                .resource::<AudioGraphRes>()
                .0
                .node_as::<tutti_nodes::DistortionNode>(node)
                .unwrap()
                .drive()
                .load(std::sync::atomic::Ordering::Acquire)
        };

        // A beat-synced source derives its phase from the transport *beat*, which
        // is its own atomic — advancing `steady_time` alone leaves it at zero and
        // the source frozen.
        let mut seen: Vec<f32> = Vec::new();
        for i in 0..16 {
            let transport = app.world().resource::<TransportRes>().clone();
            transport.settings.set_beat(Beat(i as f64 / 8.0));
            app.update();
            let v = drive(&app);
            if !seen.iter().any(|s| (s - v).abs() < 1e-3) {
                seen.push(v);
            }
        }

        assert!(
            seen.len() > 2,
            "a declined curve must fall back to scalar delivery and still modulate, saw {seen:?}"
        );
    }
}

/// A modulator kind the adapter has never heard of, driving a real param.
///
/// The registry's whole claim is that `tutti-mod`'s genericity survives into
/// the ECS layer: `Modulator` is generic over its state, `Sourced<M>` erases
/// `M`, so an app should be able to add a kind without bevy-tutti knowing it.
/// Registering only types the adapter already ships would not test that — this
/// defines a modulator here, in the test, and drives a node with it.
/// (Was `tests/mod_source.rs`.)
mod mod_source {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::modulation::{
        ModParamRange, ModRoute, ModSource, ModSourceAppExt, ModSourceKind, ModSourceRate,
        ModTargetRegistry, ModulationMatrix, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::transport::Transport;
    use tutti_core::AudioNode;
    use tutti_mod::Modulator;
    use tutti_nodes::DistortionNode;
    use tutti_types::{Depth, Hz, ParamAddr, Phase, UnitParam};

    /// A modulator with no analogue in `tutti-mod`: a two-step stair, held for
    /// half a cycle each. Stateless, so its `State` is `()` — the simplest thing
    /// the trait allows, and enough to prove the erasure works.
    struct Stair;

    impl Modulator for Stair {
        type State = ();

        fn value(&self, _state: (), phase: Phase) -> ((), f32) {
            ((), if phase.get() < 0.5 { -1.0 } else { 1.0 })
        }
    }

    /// The ECS declaration of a `Stair`, carrying a parameter `tutti-mod` has no
    /// concept of — proof that a kind owns its own config rather than squeezing
    /// into a shared `ModSource`.
    #[derive(Component, Clone)]
    struct StairSource {
        /// Scales the stair's two levels. Nothing in bevy-tutti knows this exists.
        amount: f32,
    }

    impl ModSourceKind for StairSource {
        type Source = ScaledStair;

        fn build(&self) -> ScaledStair {
            ScaledStair {
                amount: self.amount,
            }
        }
    }

    struct ScaledStair {
        amount: f32,
    }

    impl Modulator for ScaledStair {
        type State = ();

        fn value(&self, _state: (), phase: Phase) -> ((), f32) {
            let (_, v) = Stair.value((), phase);
            ((), v * self.amount)
        }
    }

    const BASE_DRIVE: f32 = 5.0;

    fn app_with_node() -> (App, Entity) {
        let mut app = App::new();

        let mut net = Net::new(0, 1);
        let node = net.push(Box::new(DistortionNode::new(
            tutti_nodes::ShapeKind::Tanh,
            1.0,
        )));
        net.pipe_output(node);

        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        let target = app.world_mut().spawn(AudioNode(node)).id();
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                BASE_DRIVE,
                0.0,
                10.0,
            ));
        (app, target)
    }

    fn node_drive(app: &App, entity: Entity) -> f32 {
        let node = app.world().get::<AudioNode>(entity).unwrap().0;
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .node_as::<DistortionNode>(node)
            .unwrap()
            .drive()
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn advance_transport(app: &mut App, samples: i64) {
        let transport = app.world().resource::<TransportRes>().clone();
        let current = transport.settings.steady_time();
        transport
            .settings
            .steady_time
            .store(current + samples, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn a_custom_kind_drives_a_param() {
        let (mut app, target) = app_with_node();
        app.add_mod_source::<StairSource>();

        let source = app
            .world_mut()
            .spawn((
                StairSource { amount: 1.0 },
                ModSourceRate::free_running(Hz(10.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
        );

        app.update();
        assert_eq!(
            app.world().resource::<ModulationMatrix>().len(),
            1,
            "the custom kind should have resolved a target"
        );

        // The stair steps at half a cycle; sweep a full one and require the drive
        // to visit two distinct levels.
        let mut seen: Vec<f32> = Vec::new();
        for _ in 0..20 {
            advance_transport(&mut app, 480);
            app.update();
            let v = node_drive(&app, target);
            if !seen.iter().any(|s| (s - v).abs() < 1e-3) {
                seen.push(v);
            }
        }

        assert!(
            seen.len() >= 2,
            "a stair should have driven the param to two levels, saw {seen:?}"
        );
        assert!(
            seen.iter().any(|v| *v > BASE_DRIVE),
            "one level should sit above base: {seen:?}"
        );
        assert!(seen.iter().any(|v| *v < BASE_DRIVE), "one below: {seen:?}");
    }

    /// The kind's own parameters must reach the built modulator — otherwise the
    /// registry is just a type-level ceremony over a fixed source.
    #[test]
    fn the_kinds_own_config_reaches_the_modulator() {
        let mut depths = Vec::new();
        for amount in [0.25_f32, 1.0] {
            let (mut app, target) = app_with_node();
            app.add_mod_source::<StairSource>();

            let source = app
                .world_mut()
                .spawn((
                    StairSource { amount },
                    ModSourceRate::free_running(Hz(10.0)),
                ))
                .id();
            app.world_mut().spawn(
                ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.2)),
            );

            let mut extreme: f32 = 0.0;
            for _ in 0..20 {
                advance_transport(&mut app, 480);
                app.update();
                extreme = extreme.max((node_drive(&app, target) - BASE_DRIVE).abs());
            }
            depths.push(extreme);
        }

        assert!(
            depths[1] > depths[0] * 2.0,
            "amount 1.0 should swing far wider than 0.25: {depths:?}"
        );
    }

    /// The built-in kind and a custom one coexist: two registered kinds means two
    /// collectors, and both must land in the one registry the routes index into.
    ///
    /// They drive the *same* param, since a distortion node exposes only `Drive`.
    /// That also exercises the summing path — two sources, two layers, one
    /// accumulator — across a kind boundary.
    #[test]
    fn a_built_in_and_a_custom_kind_coexist() {
        let (mut app, target) = app_with_node();
        app.add_mod_source::<StairSource>();

        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(bevy_tutti::modulation::LfoShape::Sine),
                ModSourceRate::free_running(Hz(10.0)),
            ))
            .id();
        let stair = app
            .world_mut()
            .spawn((
                StairSource { amount: 1.0 },
                ModSourceRate::free_running(Hz(10.0)),
            ))
            .id();

        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );
        app.world_mut().spawn(
            ModRoute::new(stair, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );

        app.update();
        assert!(app
            .world()
            .resource::<ModulationMatrix>()
            .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)));

        // Both contribute: the stair alone would step between two levels, so a
        // third distinct value can only come from the sine summing with it.
        let mut seen: Vec<f32> = Vec::new();
        for _ in 0..40 {
            advance_transport(&mut app, 480);
            app.update();
            let v = node_drive(&app, target);
            if !seen.iter().any(|s| (s - v).abs() < 1e-3) {
                seen.push(v);
            }
        }
        assert!(
            seen.len() > 2,
            "two summed sources should visit more than the stair's own two levels, saw {seen:?}"
        );
    }

    /// Registering a kind twice must schedule one collector. Two would each push a
    /// source for the same entity; the second would take a registry index no route
    /// points at, and its modulation would silently never apply.
    #[test]
    fn registering_a_kind_twice_is_idempotent() {
        let (mut app, target) = app_with_node();
        app.add_mod_source::<StairSource>()
            .add_mod_source::<StairSource>();

        let source = app
            .world_mut()
            .spawn((
                StairSource { amount: 1.0 },
                ModSourceRate::free_running(Hz(10.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
        );

        // `rebuild` drains the collected sources, so inspect them before it runs:
        // a doubly-registered kind builds two sources for the one entity, and the
        // second takes a registry index no route points at.
        app.world_mut().run_schedule(bevy_app::Update);
        let collected = app
            .world()
            .resource::<bevy_tutti::modulation::CollectedModSources>();
        assert!(
            collected.len() <= 1,
            "one registration's worth of sources, not {}",
            collected.len()
        );

        // And the route still resolves, which a duplicate index would break.
        app.update();
        assert!(app
            .world()
            .resource::<ModulationMatrix>()
            .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)));
    }
}

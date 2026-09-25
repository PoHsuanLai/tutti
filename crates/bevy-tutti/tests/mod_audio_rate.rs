//! Audio-rate modulation: the per-sample tier, end to end.
//!
//! Two scenarios, one per module below. The reconciler half asserts what a
//! `ModRoute` marked `at_audio_rate` *builds* — source → shaper → sum → param
//! port, and the teardown when the route goes away. The tier-parity half
//! asserts what the built chain *computes*, against the `tutti_mod` functions
//! the frame-rate path applies to the same route.
//!
//! They are together because a chain that is wired correctly but sums
//! differently from the value path is as much a bug as one that is not wired
//! at all, and neither half catches the other's.
//!
//! Every import below is behind `modulation`, so without the feature this file
//! does not compile rather than silently finding no tests.

#![cfg(feature = "modulation")]

#[macro_use]
mod common;

/// The audio-rate reconciler: a `ModRoute` marked `at_audio_rate` becomes a
/// real graph chain, and stops being one when the route goes away.
/// (Was `tests/mod_audio_rate_reconcile.rs`.)
mod mod_audio_rate_reconcile {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::GraphSource;
    use bevy_tutti::graph::{AudioGraphRes, CapturedControls, GraphReconcilePlugin};
    use bevy_tutti::modulation::audio_rate::{AudioRateChains, ModSourceNode};
    use bevy_tutti::modulation::{
        ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::AudioNode;
    use tutti_mod::LfoShape;
    use tutti_nodes::{DistortionNode, ParamPorts, ShapeKind};
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    /// An app with the engine's plugins and one ported distortion, ready to modulate.
    fn app_with_target() -> (App, Entity, usize) {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        // Born with its drive port on — the trigger policy this crate settled on.
        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
        let drive_port = dist.param_port(UnitParam::Drive).unwrap();
        // Declared from the unit, at the direct `Net::add` site — see
        // `bevy_tutti::graph::param_ports`.
        let ports = bevy_tutti::graph::ParamPortMap::of(&dist);
        // And its controls, captured from the unit before it moves — the same
        // step every insertion path in `bevy_tutti::graph` runs.
        let controls = CapturedControls::capture(app.world(), &dist);
        let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(dist);

        let mut target = app.world_mut().spawn((
            ports,
            ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0),
        ));
        controls.bind(&mut target, node);
        let target = target.id();

        (app, target, drive_port)
    }

    fn spawn_lfo(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id()
    }

    fn node_id(app: &App, entity: Entity) -> AudioNode {
        *app.world().get::<AudioNode>(entity).expect("AudioNode")
    }

    /// The headline: an `at_audio_rate` route materialises
    /// `source → shaper → sum → param port`, entirely from the declaration.
    /// **The third silent victim: a bus strip's Volume and Pan.**
    ///
    /// `BusStripNode` implements `ParamPorts` and exposes audio-rate ports for
    /// `Volume` and `Pan`, and it was **missing from the old nine-arm downcast
    /// list** exactly as both filters were. The failure is the same and just as
    /// quiet: a type absent from that list answered `None`, which is
    /// indistinguishable from "this node exposes no port", so a `PerSample`
    /// route onto a fader or a panner was downgraded to per-frame with nothing
    /// logged.
    ///
    /// This asserts the whole lowering rather than the port lookup, because
    /// "fell back to per-frame" is precisely the outcome a port-only assertion
    /// cannot see: the route stays well-formed and the value path still moves
    /// the param, so the only observable difference is whether a **graph chain
    /// exists**.
    ///
    /// # Mutation
    ///
    /// Deleting `BusStripNode`'s arm from the old `try_kinds!` list is what the
    /// bug *was*, and there is no list left to delete from — so both analogues
    /// were run against this test and both fail it:
    ///
    /// - dropping the `ParamPortMap` from the strip's spawn reproduces the old
    ///   list's behaviour for a type missing from it, and fails on the "must
    ///   lower to a graph chain" panic below;
    /// - removing `UnitParam::Volume` from `impl ParamPorts for BusStripNode`
    ///   is the closer analogue of the original omission (the type answering
    ///   `None` for a param it really ports), and fails on the `expect` above
    ///   it, where the node is asked what port it declares.
    #[test]
    fn a_bus_strips_volume_and_pan_reach_audio_rate() {
        for (param, label) in [(UnitParam::Volume, "Volume"), (UnitParam::Pan, "Pan")] {
            let mut app = App::new();
            app.insert_resource(AudioGraphRes::headless(0, 2));
            app.insert_resource(AudioEngineState::Running);
            app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
            app.world_mut()
                .resource_mut::<ModTargetRegistry>()
                .register::<tutti_nodes::BusStripNode>();

            // Both ports on: a port exists only when the node is *built* with
            // one, so `with_channels` would make this vacuous by reporting
            // `None` for a correctly-declared type.
            let strip = tutti_nodes::BusStripNode::with_param_inputs(
                tutti_types::ChannelLayout::STEREO,
                true,
                true,
            );
            let want_port = strip
                .param_port(param)
                .expect("the strip declares this port");
            let ports = bevy_tutti::graph::ParamPortMap::of(&strip);
            let controls = CapturedControls::capture(app.world(), &strip);
            let node = app
                .world_mut()
                .resource_mut::<AudioGraphRes>()
                .insert(strip);
            let mut target = app.world_mut().spawn((
                ports,
                ModParamRange::default().with(ParamAddr::Unit(param), 0.5, 0.0, 1.0),
            ));
            controls.bind(&mut target, node);
            let target = target.id();

            let lfo = spawn_lfo(&mut app);
            app.world_mut().spawn(
                ModRoute::new(lfo, target, ParamAddr::Unit(param))
                    .with_depth(Depth(0.5))
                    .per_sample(),
            );
            app.update();
            app.update();

            let chain = app
                .world()
                .resource::<AudioRateChains>()
                .get(target, ParamAddr::Unit(param))
                .unwrap_or_else(|| {
                    panic!(
                        "a PerSample route onto a bus strip's {label} must lower to a \
                         graph chain; no chain means it silently fell back to \
                         per-frame, which is the bug the downcast list shipped"
                    )
                })
                .clone();

            assert_eq!(
                chain.port, want_port,
                "{label}: the chain must feed the port the node declares, not \
                 some other input"
            );

            // And the sink's param port is actually fed by the chain's sum —
            // a chain that exists but is not connected is the same silence.
            let sum = node_id(&app, chain.sum);
            let graph = app.world().resource::<AudioGraphRes>();
            assert_eq!(
                graph.source(node, chain.port),
                GraphSource::Node(sum, 0),
                "{label}: the strip's param port must read the chain's sum"
            );
        }
    }

    #[test]
    fn an_audio_rate_route_builds_the_whole_chain() {
        let (mut app, target, drive_port) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        // Two updates: the first spawns source nodes and the chain, the second lets
        // the wire reconciler see the declarations they inserted.
        app.update();
        app.update();

        let chains = app.world().resource::<AudioRateChains>();
        let chain = chains
            .get(target, ParamAddr::Unit(UnitParam::Drive))
            .expect("a chain was built for the modulated param")
            .clone();

        assert_eq!(chain.shapers.len(), 1, "one shaper per route");
        assert_eq!(chain.port, drive_port);

        // The source gained a renderable node — the piece the value path never
        // needed.
        let source_node = app
            .world()
            .get::<ModSourceNode>(lfo)
            .expect("the source gained an LfoNode")
            .0;

        let (t, sum, base, shaper, src) = (
            node_id(&app, target),
            node_id(&app, chain.sum),
            node_id(&app, chain.base),
            node_id(&app, chain.shapers[0]),
            node_id(&app, source_node),
        );
        let graph = app.world().resource::<AudioGraphRes>();

        assert_eq!(
            graph.source(shaper, 0),
            GraphSource::Node(src, 0),
            "lfo → shaper"
        );
        assert_eq!(
            graph.source(sum, 0),
            GraphSource::Node(base, 0),
            "base → sum.0"
        );
        assert_eq!(
            graph.source(sum, 1),
            GraphSource::Node(shaper, 0),
            "shaper → sum.1"
        );
        assert_eq!(
            graph.source(t, drive_port),
            GraphSource::Node(sum, 0),
            "sum → the node's drive port"
        );
    }

    /// Two routes on one param share one sum, sized to the group — the constraint
    /// that forces grouping by `(target, param)` before anything is spawned.
    #[test]
    fn two_routes_on_one_param_share_one_sum() {
        let (mut app, target, _) = app_with_target();
        let (a, b) = (spawn_lfo(&mut app), spawn_lfo(&mut app));

        for source in [a, b] {
            app.world_mut().spawn(
                ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.25))
                    .per_sample(),
            );
        }
        app.update();
        app.update();

        let chains = app.world().resource::<AudioRateChains>();
        let chain = chains
            .get(target, ParamAddr::Unit(UnitParam::Drive))
            .expect("chain")
            .clone();

        assert_eq!(chain.shapers.len(), 2, "one shaper per route, one sum");

        let sum = node_id(&app, chain.sum);
        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(
            graph.node_inputs(sum),
            3,
            "the sum is sized to the group: one base port plus one per route"
        );
    }

    /// Deleting the route tears the chain down. Without this a removed route leaves
    /// a sum feeding a stale offset into the node forever — the audio-rate mirror of
    /// the layer-clearing the value path does.
    #[test]
    fn removing_the_route_retires_the_chain() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        let route = app
            .world_mut()
            .spawn(
                ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.5))
                    .per_sample(),
            )
            .id();
        app.update();
        app.update();

        let key = (target, ParamAddr::Unit(UnitParam::Drive));
        assert!(app
            .world()
            .resource::<AudioRateChains>()
            .0
            .contains_key(&key));

        app.world_mut().entity_mut(route).despawn();
        app.update();

        assert!(
            !app.world()
                .resource::<AudioRateChains>()
                .0
                .contains_key(&key),
            "the chain must be retired with its route"
        );
    }

    /// A route left at the default (value path) builds nothing. Audio rate is
    /// opt-in, because it costs two idle graph nodes per modulated param.
    #[test]
    fn a_value_path_route_builds_no_chain() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        // No `.per_sample()`.
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
        );
        app.update();
        app.update();

        assert!(
            app.world().resource::<AudioRateChains>().0.is_empty(),
            "the value path must not spawn graph nodes"
        );
        assert!(
            app.world().get::<ModSourceNode>(lfo).is_none(),
            "and its source must not gain a node it does not need"
        );
    }

    /// **The bug the enum exists to prevent.**
    ///
    /// A per-sample route is delivered as a graph chain feeding the sink's param
    /// port. If `rebuild` *also* gave it a `ModEdge`, the driver would flush
    /// `base + Σ offsets` into the node's atomic every frame while the sum drove its
    /// port — two writers over one param.
    ///
    /// With the old independent bools this was not merely possible but the default:
    /// `at_audio_rate` was invisible to `rebuild`, so every audio-rate route got
    /// both. `ModDelivery` makes the tiers mutually exclusive by construction, and
    /// this pins the driver actually honouring that.
    #[test]
    fn a_per_sample_route_is_not_also_claimed_by_the_driver() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        // The chain exists...
        assert!(
            app.world()
                .resource::<AudioRateChains>()
                .is_audio_rate(target, ParamAddr::Unit(UnitParam::Drive)),
            "the per-sample chain must be built"
        );
        // ...and the frame-rate driver has NOT claimed the same param.
        assert!(
            !app.world()
                .resource::<bevy_tutti::modulation::ModulationMatrix>()
                .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
            "the driver must not also own a param delivered per sample — that is \
             two writers on one atomic"
        );
    }

    /// The complement: a per-frame route *is* the driver's, and builds no chain.
    /// Together these pin the two tiers as mutually exclusive in both directions.
    #[test]
    fn a_per_frame_route_is_the_drivers_alone() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
        );
        app.update();
        app.update();

        assert!(
            app.world()
                .resource::<bevy_tutti::modulation::ModulationMatrix>()
                .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
            "the driver owns a per-frame param"
        );
        assert!(
            app.world().resource::<AudioRateChains>().0.is_empty(),
            "and no graph chain is built for it"
        );
    }

    /// **A route declared before its sink's node still reaches audio rate.**
    ///
    /// The ordering a real host produces, and the one the reconciler used to fail
    /// on. `spawn_chain` resolves the sink's param port through its `AudioNode`, so
    /// a route whose sink has no node yet correctly builds nothing — but the gate
    /// watched only `Changed<ModRoute>`/`Changed<ModParamRange>`, so when the node
    /// arrived nothing asked again and the route stayed on the per-frame fallback
    /// **permanently**.
    ///
    /// This is the ordinary order, not a contrived one. A host compiling a document
    /// declares routes and spawns nodes in the same frame, and `insert_audio_node`
    /// lands as a *deferred* command — so the route is visible one frame before the
    /// `AudioNode` is.
    ///
    /// It failed silently, which is why it needed a test rather than a review: the
    /// per-frame fallback is a legal outcome meaning "this sink exposes no port",
    /// and nothing distinguishes it from "the node had not arrived yet".
    #[test]
    fn a_route_declared_before_its_sinks_node_still_binds() {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        // The sink exists as an entity with its declared range, but carries **no**
        // `AudioNode` yet — exactly what a projection produces before the spawner
        // has run.
        let target = app
            .world_mut()
            .spawn(ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0))
            .id();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );

        app.update();
        app.update();
        assert!(
            app.world()
                .resource::<AudioRateChains>()
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .is_none(),
            "with no node on the sink there is no port to resolve, so no chain — \
             this half must hold or the assertion below proves nothing"
        );

        // The node arrives a frame later, as a deferred insert would.
        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
        let drive_port = dist.param_port(UnitParam::Drive).unwrap();
        let ports = bevy_tutti::graph::ParamPortMap::of(&dist);
        let controls = CapturedControls::capture(app.world(), &dist);
        let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(dist);
        let mut sink = app.world_mut().entity_mut(target);
        sink.insert(ports);
        controls.bind(&mut sink, node);

        app.update();
        app.update();

        let chain = app
            .world()
            .resource::<AudioRateChains>()
            .get(target, ParamAddr::Unit(UnitParam::Drive))
            .expect(
                "the sink's node arrived, so the route must now bind — a node \
                 appearing after its route is the ordinary order, not an edge case",
            )
            .clone();
        assert_eq!(chain.port, drive_port);
    }

    /// **Editing a modulated param's authored range must not delete its modulation.**
    ///
    /// `rebuild` runs when a route *or a range* changes, and builds its source
    /// registry from `CollectedModSources::sources` — which it **drains**. But the
    /// per-kind `collect` systems refill that list only when `collected.dirty`, and
    /// `dirty` tracks *source* changes. So a rebuild triggered by a range change
    /// alone found an empty registry, failed `source_index.get(..)` for every route,
    /// and dropped every accumulator.
    ///
    /// The user-visible effect: turning the knob on a modulated parameter silently
    /// removes its LFO. Nothing errors — the matrix simply empties.
    ///
    /// A range change is not exotic. `ModParamRange` carries the authored
    /// `base`/`min`/`max`, so any host that mirrors an authored value into it
    /// re-inserts the component on every edit.
    #[test]
    fn editing_a_range_does_not_drop_the_routes() {
        let (mut app, target, _port) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
        );
        app.update();
        app.update();
        assert!(
            app.world()
                .resource::<bevy_tutti::modulation::ModulationMatrix>()
                .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
            "the route must bind first, or the assertion below is vacuous"
        );

        // Re-declare the range with a new base — what a host does when the user
        // moves the authored value of a modulated param.
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                7.0,
                0.0,
                10.0,
            ));
        app.update();

        assert!(
            app.world()
                .resource::<bevy_tutti::modulation::ModulationMatrix>()
                .is_modulated(target, ParamAddr::Unit(UnitParam::Drive)),
            "the route must survive a range edit — a rebuild that drains its source \
             registry without refilling it drops every route it cannot resolve"
        );
    }

    /// **`write_param` routes an authored write to the chain's base cell.**
    ///
    /// The branch-level guard, and it has to be here. A host's end-to-end test
    /// cannot supply it: a *document* edit re-declares `ModParamRange`, which
    /// `reconcile_audio_rate` also folds into the cell, so the two paths are
    /// redundant there and neither sabotage alone fails it (measured).
    ///
    /// Here there is no `declare_param_ranges` in the loop. The write is made
    /// directly, against a range the "document" never moved, so only
    /// `write_param`'s audio-rate branch can deliver it.
    ///
    /// # Why the assertion is on the cell and not the node's atomic
    ///
    /// Because the node's atomic is a decoy once a param port is wired: it holds
    /// whatever was last written there while the DSP reads the port.
    /// `tutti-nodes`' `a_wired_param_port_makes_the_node_ignore_its_atomic` pins
    /// that. Asserting on the atomic would pass with the branch removed — the write
    /// lands there, it just does not sound.
    #[test]
    fn write_param_reaches_an_audio_rate_params_base_cell() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        let cell = app
            .world()
            .resource::<AudioRateChains>()
            .base_cell(target, ParamAddr::Unit(UnitParam::Drive))
            .expect("the per-sample chain must exist");
        assert_eq!(
            cell.load(tutti_core::Ordering::Acquire),
            5.0,
            "the chain starts at the declared base"
        );

        // An authored write through the real front door — no ModParamRange edit, so
        // the reconciler's refresh cannot be what delivers it.
        let node = *app.world().get::<AudioNode>(target).unwrap();
        app.world_mut()
            .resource_scope(|w, mut graph: Mut<AudioGraphRes>| {
                let matrix = w.resource::<bevy_tutti::modulation::ModulationMatrix>();
                let chains = w.resource::<AudioRateChains>();
                bevy_tutti::graph::write_param(
                    &mut graph,
                    matrix,
                    chains,
                    target,
                    &node,
                    UnitParam::Drive,
                    9.0,
                );
            });

        assert_eq!(
            cell.load(tutti_core::Ordering::Acquire),
            9.0,
            "write_param must land on the chain's base cell; an unchanged cell \
             means the write went to the node's own atomic, which a wired param \
             port ignores"
        );
    }

    /// **A range edit reaches a live chain without respawning it.**
    ///
    /// The other half of the base path: `declare_param_ranges`-style edits arrive
    /// as a new `ModParamRange`, and `reconcile_audio_rate` must fold the new base
    /// into the existing chain's cell.
    ///
    /// Both halves of the assertion matter. Without the first, the base is frozen
    /// at its spawn value for the chain's whole life — which is what shipped.
    /// Without the second, the "fix" of respawning the chain would pass while
    /// restarting every LFO's phase, which is the thing the arity check exists to
    /// prevent.
    #[test]
    fn a_range_edit_reaches_a_live_chain_without_respawning_it() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        let before: Vec<Entity> = {
            let chains = app.world().resource::<AudioRateChains>();
            let chain = chains
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .expect("the chain must exist");
            std::iter::once(chain.base)
                .chain(std::iter::once(chain.sum))
                .chain(chain.shapers.iter().copied())
                .collect()
        };
        let cell = app
            .world()
            .resource::<AudioRateChains>()
            .base_cell(target, ParamAddr::Unit(UnitParam::Drive))
            .unwrap();

        // Re-declare the range with a new base — the same shape a document edit
        // takes, and the same route count, so the arity check will skip it.
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                9.0,
                0.0,
                10.0,
            ));
        app.update();

        assert_eq!(
            cell.load(tutti_core::Ordering::Acquire),
            9.0,
            "the new authored base must reach the live chain's cell"
        );

        let after: Vec<Entity> = {
            let chains = app.world().resource::<AudioRateChains>();
            let chain = chains
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .expect("the chain must still exist");
            std::iter::once(chain.base)
                .chain(std::iter::once(chain.sum))
                .chain(chain.shapers.iter().copied())
                .collect()
        };
        assert_eq!(
            before, after,
            "a base edit must not respawn the chain — respawning restarts every \
             LFO's phase, which is exactly what the arity check protects"
        );
    }

    /// **A depth edit on a live route reaches its shaper.**
    ///
    /// `ParamShaperNode` bakes depth, polarity and curve into a LUT at construction
    /// and has no setter, and the reconciler's shape test is the group's *arity* —
    /// so before this was fixed, a depth slider changed the declaration and nothing
    /// else. The chain kept rendering with the depth it was born with, for its
    /// whole life. A dead control, exactly like the frozen base beside it.
    ///
    /// # The assertions, and why each is needed
    ///
    /// - The **shaper's output moved**: read by ticking the live node, not by
    ///   trusting the declaration. This is the bug.
    /// - The **sum and base survived**: a whole-chain respawn would also make the
    ///   first assertion pass while silently reverting the authored base to
    ///   `ModParamRange`'s, which is the regression this shape of fix invites.
    /// - The **LFO's node survived**: the module's anti-respawn note protects the
    ///   modulator's phase, and this pins that a shaper swap does not touch it.
    #[test]
    fn a_depth_edit_reaches_a_live_shaper() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        let route = app
            .world_mut()
            .spawn(
                ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(0.1))
                    .per_sample(),
            )
            .id();
        app.update();
        app.update();

        /// The shaper's offset at full-scale input — its effective depth.
        fn shaped(app: &App, target: Entity) -> f32 {
            let chains = app.world().resource::<AudioRateChains>();
            let chain = chains
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .expect("the chain must exist");
            let node = *app.world().get::<AudioNode>(chain.shapers[0]).unwrap();
            let graph = app.world().resource::<AudioGraphRes>();
            let mut unit = graph
                .inspect(node, |unit| {
                    unit.as_any()
                        .downcast_ref::<tutti_nodes::ParamShaperNode>()
                        .cloned()
                })
                .flatten()
                .expect("the shaper is a ParamShaperNode");
            let mut out = [0.0f32; 1];
            tutti_core::AudioUnit::tick(&mut unit, &[1.0], &mut out);
            out[0]
        }

        let before = shaped(&app, target);
        assert!(
            (before - 0.1).abs() < 1e-3,
            "the chain starts at its authored depth; got {before}"
        );

        let (sum_before, base_before, lfo_node_before) = {
            let chains = app.world().resource::<AudioRateChains>();
            let chain = chains
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .unwrap();
            let lfo_node = app
                .world()
                .get::<ModSourceNode>(lfo)
                .expect("the LFO must have a node")
                .0;
            (chain.sum, chain.base, lfo_node)
        };

        app.world_mut().get_mut::<ModRoute>(route).unwrap().depth = Depth(0.8);
        app.update();

        let after = shaped(&app, target);
        assert!(
            (after - 0.8).abs() < 1e-3,
            "the edited depth must reach the live shaper; got {after} (was \
             {before}). An unchanged value means the declaration moved and the \
             rendered node did not."
        );

        let chains = app.world().resource::<AudioRateChains>();
        let chain = chains
            .get(target, ParamAddr::Unit(UnitParam::Drive))
            .unwrap();
        assert_eq!(
            (chain.sum, chain.base),
            (sum_before, base_before),
            "only the shaper may be rebuilt — respawning the whole chain would \
             revert the authored base to whatever ModParamRange last declared"
        );
        assert_eq!(
            app.world().get::<ModSourceNode>(lfo).unwrap().0,
            lfo_node_before,
            "the LFO's node must survive: respawning it restarts its phase, which \
             is what the anti-respawn policy actually protects"
        );
    }

    /// **A range edit reaches a live chain's clamp.**
    ///
    /// The third and last of the frozen-at-construction bugs on this path, after
    /// the base and the shaper. `ParamSumNode` held `min`/`max` as plain fields, so
    /// narrowing a param's range moved the declaration and nothing else — the sum
    /// went on clamping to the range it was born with.
    ///
    /// Asserts the entities are unchanged for the same reason
    /// `a_range_edit_reaches_a_live_chain_without_respawning_it` does: rebuilding
    /// the sum would apply the new clamp *and* silently revert the authored base,
    /// so a test that only checked the clamp would bless that trade.
    #[test]
    fn a_range_edit_reaches_a_live_clamp() {
        let (mut app, target, _) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        /// What the live sum clamps `base` to — **rendered**, not inspected:
        /// the chain's base cell is set to `base`, the sum is put on global
        /// output 0, and one frame of the graph is rendered.
        ///
        /// Rendered because the clamp lives in `ClampBounds`, a cell the live
        /// sum shares with the chain, and not in anything a `Setting` carries:
        /// an inspected copy of the node shows it only while that copy shares
        /// the cell, which the node's shadow does not once `isolate`
        /// snapshots cells (#29). The offset on the other port is the LFO
        /// through a 0.5-deep shaper, a few units at most, so a base far past
        /// the max reads back as the max.
        fn clamped(app: &mut App, target: Entity, base: f32) -> f32 {
            let (sum, cell) = {
                let chains = app.world().resource::<AudioRateChains>();
                let chain = chains
                    .get(target, ParamAddr::Unit(UnitParam::Drive))
                    .expect("the chain must exist");
                (
                    *app.world().get::<AudioNode>(chain.sum).unwrap(),
                    chain.base_cell(),
                )
            };
            cell.store(base, std::sync::atomic::Ordering::Release);
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.set_output_source(0, GraphSource::Node(sum, 0));
            let mut out = [0.0f32; 2];
            graph.render_frame(&mut out);
            out[0]
        }

        assert_eq!(
            clamped(&mut app, target, 100.0),
            10.0,
            "the chain starts clamped to its declared max"
        );

        let (sum_before, base_before) = {
            let chains = app.world().resource::<AudioRateChains>();
            let chain = chains
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .unwrap();
            (chain.sum, chain.base)
        };

        // Narrow the declared range: 0..=10 becomes 0..=2.
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                1.0,
                0.0,
                2.0,
            ));
        app.update();

        assert_eq!(
            clamped(&mut app, target, 100.0),
            2.0,
            "the narrowed range must reach the live sum; still clamping at 10 \
             means the declaration moved and the rendered node did not"
        );

        let chains = app.world().resource::<AudioRateChains>();
        let chain = chains
            .get(target, ParamAddr::Unit(UnitParam::Drive))
            .unwrap();
        assert_eq!(
            (chain.sum, chain.base),
            (sum_before, base_before),
            "the clamp must move in place — rebuilding the sum would also revert \
             the authored base, trading one frozen value for another"
        );
    }
}

/// The two tiers agree: audio-rate modulation sounds like the frame-rate path.
///
/// The matrix delivers a native-param route either per frame (an `AtomicTarget`
/// mirroring into the node's atomic) or per sample (`ParamShaperNode →
/// ParamSumNode → node.param_port`). A route can switch tiers, so the two must
/// compute the same value — a divergence would be heard as a level or timbre
/// jump on a change that is supposed to be inaudible.
///
/// Both tests here are **numeric**: they compare a node's `tick` against the
/// `tutti_mod` function the frame-rate accumulator uses, with no `App` and no
/// reconciler. What the *reconciler* emits is `mod_audio_rate_reconcile.rs`'s
/// subject; this file is only about the arithmetic at each end.
///
/// This file used to also hold two hand-built graph-shape specs, written when
/// nothing connected a `ModRoute` to the per-sample tier. That reconciler now
/// exists, and `mod_audio_rate_reconcile.rs` asserts the same edges on a chain
/// it actually emitted, so the hand-built pair was deleted rather than left
/// claiming coverage of a graph nothing built.
/// (Was `tests/mod_tier_parity.rs`.)
mod mod_tier_parity {
    use bevy_ecs::prelude::*;

    use bevy_tutti::modulation::ModRoute;
    use tutti_core::AudioUnit as _;
    use tutti_mod::{shape, CurveType, Polarity};
    use tutti_nodes::{ParamShaperNode, ParamSumNode};
    use tutti_types::{Depth, ParamAddr, UnitParam};

    /// A shaper built from a route's own fields shapes like the route does.
    ///
    /// Two claims in one, and the second is what can fail at runtime:
    ///
    /// - Every input `ParamShaperNode::new` takes is a field already on `ModRoute`,
    ///   so the translation needs no new authoring vocabulary. (That half is a
    ///   compile-time fact, and the reconciler's own source now depends on it.)
    /// - The node's per-sample output matches `tutti_mod::shape` — the exact
    ///   function the frame-rate accumulator applies to the same route. The shaper
    ///   bakes depth, polarity and curve into a LUT, so this is a real comparison
    ///   between an interpolated table and the closed form, not a tautology.
    ///
    /// `Exponential` rather than `Linear`: a linear curve would agree with almost
    /// any LUT, so the curved case is the one that discriminates.
    #[test]
    fn a_shaper_built_from_a_route_agrees_with_the_routes_control_rate_shaping() {
        let mut world = World::new();
        let (src, dst) = (world.spawn_empty().id(), world.spawn_empty().id());

        let route = ModRoute::new(src, dst, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.5))
            .with_polarity(Polarity::Unipolar)
            .with_curve(CurveType::Exponential);

        // The shaper is built straight from the route's fields.
        let shaper = ParamShaperNode::new(route.depth, route.polarity, route.curve);

        // ...and it agrees with the control-rate shaping of the same route, which is
        // what keeps a route's sound stable if delivery ever switches tiers.
        for x in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
            let mut got = [0.0f32; 1];
            let mut s = shaper.clone();
            s.tick(&[x], &mut got);
            let want = shape(x, route.depth, route.polarity, route.curve);
            assert!(
                (got[0] - want).abs() < 1e-3,
                "audio-rate shaping diverged from the route's control-rate shaping \
                 at {x}: {} vs {want}",
                got[0]
            );
        }
    }

    /// The arithmetic the chain performs, verified against the same `fold` the
    /// frame-rate accumulator uses. Two full-scale sources at depth 0.25 land the
    /// same offset whichever tier evaluates them.
    #[test]
    fn the_chain_sums_to_what_the_frame_rate_path_would() {
        let depth = Depth(0.25);
        let (base, min, max) = (5.0f32, 0.0f32, 10.0f32);

        // Audio-rate: two shapers into a sum.
        let mut sum = ParamSumNode::new(2, min, max);
        let shaped = shape(1.0, depth, Polarity::Bipolar, CurveType::Linear);
        let mut out = [0.0f32; 1];
        sum.tick(&[base, shaped, shaped], &mut out);

        // Frame-rate: the same offsets folded by tutti-mod.
        let want = tutti_mod::fold(base, [shaped, shaped].into_iter(), min, max);

        assert!(
            (out[0] - want).abs() < 1e-6,
            "audio-rate sum {} disagrees with the control-rate fold {want}",
            out[0]
        );
    }
}

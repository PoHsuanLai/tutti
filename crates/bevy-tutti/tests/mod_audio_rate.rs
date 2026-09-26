//! Audio-rate modulation: the per-sample tier, end to end.
//!
//! Two scenarios, one per module below. The reconciler half asserts what a
//! `ModRoute` marked `per_sample` *declares* — the source's node through the
//! route's shaping into the target's param, in the graph's own value — and
//! the retirement when the route goes away. The tier-parity half asserts what
//! the graph *computes* for it, against the `tutti_mod` functions the
//! frame-rate path applies to the same route.
//!
//! They are together because a declaration that is correct but sums
//! differently from the value path is as much a bug as one that is not made
//! at all, and neither half catches the other's.
//!
//! The per-sample tier used to build a sub-graph per param (an
//! `AtomicSourceNode` base, a `ParamSumNode`, a `ParamShaperNode` per route,
//! into ports the node had to be born with); the graph now owns that
//! arithmetic (design doc 013 item 6), so each test here asserts on the graph
//! value (`AudioGraphRes::param_mod`) and on what renders, where it used to
//! assert on chain entities. Every property the chain tests pinned is still
//! pinned; each test says which one it carries.
//!
//! Every import below is behind `modulation`, so without the feature this file
//! does not compile rather than silently finding no tests.

#![cfg(feature = "modulation")]

#[macro_use]
mod common;

/// The audio-rate reconciler: a `ModRoute` marked `per_sample` becomes a
/// param modulation in the graph, and stops being one when the route goes
/// away. (Was `tests/mod_audio_rate_reconcile.rs`.)
mod mod_audio_rate_reconcile {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{AudioGraphRes, CapturedControls, GraphReconcilePlugin, GraphSource};
    use bevy_tutti::modulation::audio_rate::{AudioRateRoutes, ModSourceNode};
    use bevy_tutti::modulation::{
        ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry, TuttiModulationPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::AudioNode;
    use tutti_graph::{ParamFrom, ParamMod};
    use tutti_mod::LfoShape;
    use tutti_nodes::{DistortionNode, ShapeKind};
    use tutti_types::graph::{NodeKey, OutPort};
    use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

    /// An app with the engine's plugins and one distortion, ready to modulate.
    fn app_with_target() -> (App, Entity) {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        // Any distortion: its drive is modulatable by the graph whatever it
        // was built with, so there is no port to be born with any more.
        let dist = DistortionNode::new(ShapeKind::Tanh, 5.0);
        // Its controls, captured from the unit before it moves — the same
        // step every insertion path in `bevy_tutti::graph` runs.
        let controls = CapturedControls::capture(app.world(), &dist);
        let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(dist);

        let mut target = app.world_mut().spawn(ModParamRange::default().with(
            ParamAddr::Unit(UnitParam::Drive),
            5.0,
            0.0,
            10.0,
        ));
        controls.bind(&mut target, node);
        let target = target.id();

        (app, target)
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

    /// The graph's declared modulation of `param` on `target`'s node.
    fn param_mod(app: &App, target: Entity, param: UnitParam) -> Option<ParamMod> {
        let node = node_id(app, target);
        app.world()
            .resource::<AudioGraphRes>()
            .param_mod(node, param)
    }

    /// `node`'s output 0 as a param source.
    fn from(node: AudioNode) -> ParamFrom {
        ParamFrom::Audio(OutPort {
            node: NodeKey(node.0.value()),
            port: 0,
        })
    }

    /// The source node the audio-rate tier gave `lfo`.
    fn lfo_node(app: &App, lfo: Entity) -> AudioNode {
        let e = app
            .world()
            .get::<ModSourceNode>(lfo)
            .expect("the source gained an LfoNode")
            .0;
        node_id(app, e)
    }

    /// The base the graph's modulation of `param` rides on: the node's own
    /// control, read through the unit's `param_base` on its shadow (which
    /// every `set_param` reaches). No downcast.
    fn node_base(app: &App, target: Entity) -> f32 {
        let node = node_id(app, target);
        app.world()
            .resource::<AudioGraphRes>()
            .inspect(node, |u| u.param_base(0))
            .flatten()
            .expect("the distortion answers its base")
    }

    /// **A bus strip's Volume and Pan reach audio rate.**
    ///
    /// The third victim of the old port lookup (the strip was missing from a
    /// hand-kept downcast list, as both filters were, and the route fell back
    /// to per-frame with nothing logged). The strip now declares its params
    /// itself (`STRIP_PARAMS`), and the graph is asked, so there is no list
    /// to leave a type out of.
    ///
    /// Asserts the whole declaration rather than a lookup, because "fell back
    /// to per-frame" is precisely what a lookup-only assertion cannot see:
    /// the route stays well-formed and the value path still moves the param.
    ///
    /// Mutation (run): remove `UnitParam::Volume` from `STRIP_PARAMS` (the
    /// strip answering "not modulatable" for a param it reads) → the Volume
    /// case finds no modulation → fails.
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

            let strip =
                tutti_nodes::BusStripNode::with_channels(tutti_types::ChannelLayout::STEREO);
            let controls = CapturedControls::capture(app.world(), &strip);
            let node = app
                .world_mut()
                .resource_mut::<AudioGraphRes>()
                .insert(strip);
            assert!(
                app.world()
                    .resource::<AudioGraphRes>()
                    .declares_param(node, param),
                "the strip declares {label} modulatable"
            );
            let mut target = app.world_mut().spawn(ModParamRange::default().with(
                ParamAddr::Unit(param),
                0.5,
                0.0,
                1.0,
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

            assert!(
                app.world()
                    .resource::<AudioRateRoutes>()
                    .is_audio_rate(target, ParamAddr::Unit(param)),
                "a PerSample route onto a bus strip's {label} must reach audio rate; \
                 none means it silently fell back to per-frame"
            );
            let m = param_mod(&app, target, param).unwrap_or_else(|| {
                panic!("{label}: the graph must hold the modulation, not just the resource")
            });
            assert_eq!(
                m.sources.iter().map(|s| s.from).collect::<Vec<_>>(),
                vec![from(lfo_node(&app, lfo))],
                "{label}: the strip's param must read the route's source"
            );
        }
    }

    /// The headline: a `per_sample` route declares `source → shaping → the
    /// node's param`, entirely from the declaration, and the source gains a
    /// renderable node — the piece the value path never needed.
    ///
    /// Mutation (run): declare the group's shapings as the identity
    /// (`ParamShaping::Identity` for `s.shaping()`) → the shaping assertion
    /// fails.
    #[test]
    fn an_audio_rate_route_declares_the_whole_modulation() {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        // Two updates: the first spawns the source's node, the second
        // declares the modulation once that node is visible.
        app.update();
        app.update();

        let m = param_mod(&app, target, UnitParam::Drive)
            .expect("the modulated param is in the graph value");
        assert_eq!(m.sources.len(), 1, "one source per route");
        assert_eq!(m.sources[0].from, from(lfo_node(&app, lfo)), "lfo → drive");
        assert_eq!(
            m.sources[0].shaping,
            tutti_nodes::ParamModShaping {
                depth: Depth(0.5),
                polarity: tutti_mod::Polarity::Bipolar,
                curve: tutti_mod::CurveType::Linear,
            }
            .shaping(),
            "through the route's own shaping"
        );
        assert_eq!(
            (m.range.min, m.range.max),
            (0.0, 10.0),
            "clamped to the declared range"
        );
        assert_eq!(node_base(&app, target), 5.0, "riding on the declared base");
    }

    /// Two routes on one param are two sources of one modulation, summed by the
    /// graph — the constraint that forces grouping by `(target, param)`.
    ///
    /// Mutation (run): declare only a group's first route (`group[..1]`) →
    /// fails.
    #[test]
    fn two_routes_on_one_param_share_one_modulation() {
        let (mut app, target) = app_with_target();
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

        let m = param_mod(&app, target, UnitParam::Drive).expect("the modulation");
        let mut got: Vec<ParamFrom> = m.sources.iter().map(|s| s.from).collect();
        let mut want = vec![from(lfo_node(&app, a)), from(lfo_node(&app, b))];
        got.sort();
        want.sort();
        assert_eq!(
            got, want,
            "the group is one modulation with one source per route"
        );
    }

    /// Deleting the route retires the modulation. Without this a removed route
    /// leaves the param riding a stale offset forever — the audio-rate mirror
    /// of the layer-clearing the value path does.
    ///
    /// Mutation (run): skip the retire loop → the graph keeps the modulation
    /// → fails.
    #[test]
    fn removing_the_route_retires_the_modulation() {
        let (mut app, target) = app_with_target();
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
            .resource::<AudioRateRoutes>()
            .0
            .contains_key(&key));
        assert!(param_mod(&app, target, UnitParam::Drive).is_some());

        app.world_mut().entity_mut(route).despawn();
        app.update();

        assert!(
            !app.world()
                .resource::<AudioRateRoutes>()
                .0
                .contains_key(&key),
            "the declaration must be retired with its route"
        );
        assert!(
            param_mod(&app, target, UnitParam::Drive).is_none(),
            "and the graph must drop it: the param reads its own control again"
        );
    }

    /// A route left at the default (value path) declares nothing. Audio rate is
    /// opt-in.
    #[test]
    fn a_value_path_route_declares_no_modulation() {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        // No `.per_sample()`.
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.5)),
        );
        app.update();
        app.update();

        assert!(
            app.world().resource::<AudioRateRoutes>().0.is_empty(),
            "the value path must not declare graph modulation"
        );
        assert!(param_mod(&app, target, UnitParam::Drive).is_none());
        assert!(
            app.world().get::<ModSourceNode>(lfo).is_none(),
            "and its source must not gain a node it does not need"
        );
    }

    /// **The bug the enum exists to prevent.**
    ///
    /// A per-sample route is delivered by the graph's modulation of the param.
    /// If `rebuild` *also* gave it a `ModEdge`, the driver would flush
    /// `base + Σ offsets` into the node's atomic every frame — the very cell
    /// the graph's modulation rides on as its base — so the route would be
    /// applied twice. `ModDelivery` makes the tiers mutually exclusive by
    /// construction, and this pins the driver honouring that.
    #[test]
    fn a_per_sample_route_is_not_also_claimed_by_the_driver() {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);

        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        // The modulation is declared...
        assert!(
            app.world()
                .resource::<AudioRateRoutes>()
                .is_audio_rate(target, ParamAddr::Unit(UnitParam::Drive)),
            "the per-sample modulation must be declared"
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

    /// The complement: a per-frame route *is* the driver's, and declares no
    /// graph modulation. Together these pin the two tiers as mutually
    /// exclusive in both directions.
    #[test]
    fn a_per_frame_route_is_the_drivers_alone() {
        let (mut app, target) = app_with_target();
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
            app.world().resource::<AudioRateRoutes>().0.is_empty(),
            "and no graph modulation is declared for it"
        );
    }

    /// **A route declared before its sink's node still reaches audio rate.**
    ///
    /// The ordering a real host produces, and the one the reconciler used to fail
    /// on. The declaration needs the sink's `AudioNode`, so a route whose sink
    /// has no node yet correctly declares nothing — but if the gate watched only
    /// `Changed<ModRoute>`/`Changed<ModParamRange>`, nothing would ask again when
    /// the node arrived and the route would stay on the per-frame fallback
    /// **permanently**.
    ///
    /// Mutation (run): drop `Changed<AudioNode>` from `RouteChanged` → the
    /// route never binds → fails.
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
                .resource::<AudioRateRoutes>()
                .get(target, ParamAddr::Unit(UnitParam::Drive))
                .is_none(),
            "with no node on the sink there is nothing to modulate — this half \
             must hold or the assertion below proves nothing"
        );

        // The node arrives a frame later, as a deferred insert would.
        let dist = DistortionNode::new(ShapeKind::Tanh, 5.0);
        let controls = CapturedControls::capture(app.world(), &dist);
        let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(dist);
        let mut sink = app.world_mut().entity_mut(target);
        controls.bind(&mut sink, node);

        app.update();
        app.update();

        assert!(
            param_mod(&app, target, UnitParam::Drive).is_some(),
            "the sink's node arrived, so the route must now bind — a node \
             appearing after its route is the ordinary order, not an edge case"
        );
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
    #[test]
    fn editing_a_range_does_not_drop_the_routes() {
        let (mut app, target) = app_with_target();
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

    /// **`write_param` reaches an audio-rate param's base.**
    ///
    /// The branch-level guard. The write is made directly, against a range the
    /// "document" never moved, so only `write_param` can deliver it.
    ///
    /// The base is the node's own control now — the graph's modulation rides
    /// on it — so the assertion reads that control (through the node's
    /// shadow, which every `set_param` reaches, and which is what a fork is
    /// taken from). It used to be a base chain's cell no `Setting` reached,
    /// which a fork could not see.
    ///
    /// Mutation (run): make `write_param` return early for an audio-rate param
    /// (the old branch, with no cell to write) → the base stays at 5 → fails.
    #[test]
    fn write_param_reaches_an_audio_rate_params_base() {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();
        assert!(param_mod(&app, target, UnitParam::Drive).is_some());
        assert_eq!(node_base(&app, target), 5.0, "the declared base");

        // An authored write through the real front door — no ModParamRange edit,
        // so the reconciler cannot be what delivers it.
        let node = *app.world().get::<AudioNode>(target).unwrap();
        app.world_mut()
            .resource_scope(|w, mut graph: Mut<AudioGraphRes>| {
                let matrix = w.resource::<bevy_tutti::modulation::ModulationMatrix>();
                bevy_tutti::graph::write_param(
                    &mut graph,
                    matrix,
                    target,
                    &node,
                    UnitParam::Drive,
                    9.0,
                );
            });

        assert_eq!(
            node_base(&app, target),
            9.0,
            "write_param must land on the node's own control, the base the \
             graph's modulation rides on"
        );
    }

    /// **A range edit reaches a live modulation without respawning its source.**
    ///
    /// The other half of the base path: `declare_param_ranges`-style edits
    /// arrive as a new `ModParamRange`, and the reconciler must fold the new
    /// base into the node's control and the new range into the graph.
    ///
    /// Both halves of the assertion matter. Without the first, the base is
    /// frozen at its first value — which shipped once. Without the second, a
    /// "fix" that respawned the modulation would pass while restarting every
    /// LFO's phase.
    ///
    /// Mutation (run): write the base only when a group is first declared
    /// (drop `o.base != p.base`) → the base stays at 5 → fails.
    #[test]
    fn a_range_edit_reaches_a_live_modulation_without_respawning_it() {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        let lfo_before = lfo_node(&app, lfo);
        let sources_before = param_mod(&app, target, UnitParam::Drive)
            .expect("the modulation")
            .sources;

        // Re-declare the range with a new base — the same shape a document edit
        // takes, and the same routes.
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
            node_base(&app, target),
            9.0,
            "the new authored base must reach the node's control"
        );
        assert_eq!(
            lfo_node(&app, lfo),
            lfo_before,
            "a base edit must not respawn the source — respawning restarts every \
             LFO's phase"
        );
        assert_eq!(
            param_mod(&app, target, UnitParam::Drive)
                .expect("the modulation")
                .sources,
            sources_before,
            "and must leave the modulation's sources as they were"
        );
    }

    /// **A depth edit on a live route reaches the graph.**
    ///
    /// A depth slider changes the route and nothing else; before the shaping
    /// was compared, the modulation kept rendering with the depth it was born
    /// with, for its whole life — a dead control.
    ///
    /// # The assertions, and why each is needed
    ///
    /// - The **declared shaping moved**: read from the graph value, whose
    ///   `ShapeLut` is what renders — at full-scale input it is the depth.
    ///   This is the bug.
    /// - The **source's node survived**: the anti-respawn policy protects the
    ///   modulator's phase, and this pins that a shaping edit does not touch
    ///   it.
    ///
    /// Mutation (run): compare only sources' nodes when deciding what changed
    /// (ignore shapings) → the depth stays at 0.1 → fails.
    #[test]
    fn a_depth_edit_reaches_the_live_modulation() {
        let (mut app, target) = app_with_target();
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

        /// The declared shaping's offset at full-scale input — its depth.
        fn shaped(app: &App, target: Entity) -> f32 {
            param_mod(app, target, UnitParam::Drive)
                .expect("the modulation")
                .sources[0]
                .shaping
                .apply(1.0)
        }

        let before = shaped(&app, target);
        assert!(
            (before - 0.1).abs() < 1e-3,
            "the modulation starts at its authored depth; got {before}"
        );
        let lfo_node_before = lfo_node(&app, lfo);

        app.world_mut().get_mut::<ModRoute>(route).unwrap().depth = Depth(0.8);
        app.update();

        let after = shaped(&app, target);
        assert!(
            (after - 0.8).abs() < 1e-3,
            "the edited depth must reach the graph; got {after} (was {before}). \
             An unchanged value means the declaration moved and the rendered \
             modulation did not."
        );
        assert_eq!(
            lfo_node(&app, lfo),
            lfo_node_before,
            "the LFO's node must survive: respawning it restarts its phase, which \
             is what the anti-respawn policy actually protects"
        );
    }

    /// **A range edit reaches a live clamp.**
    ///
    /// Narrowing a param's range must move the clamp the graph applies, not
    /// only the declaration. **Rendered**, not inspected: a constant 0.5 into
    /// the distortion, the base written far past the max, and the output read
    /// once the connection's declick is over — `tanh(0.5 · max)`, whatever
    /// the LFO's few units of offset do, since the sum is clamped.
    ///
    /// Mutation (run): declare the group without its range (`ParamRange::UNBOUNDED`)
    /// → the drive is 100 + offset, not the max → fails.
    #[test]
    fn a_range_edit_reaches_a_live_clamp() {
        let (mut app, target) = app_with_target();
        let lfo = spawn_lfo(&mut app);
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.5))
                .per_sample(),
        );
        app.update();
        app.update();

        // A constant into the distortion, the distortion onto output 0.
        let target_node = node_id(&app, target);
        {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let dc = graph.insert(tutti_nodes::testing::Const::mono(0.5));
            graph.set_source(target_node, 0, GraphSource::Node(dc, 0));
            graph.set_source(target_node, 1, GraphSource::Node(dc, 0));
            graph.set_output_source(0, GraphSource::Node(target_node, 0));
        }

        /// What the distortion renders with the base written at 100: its
        /// drive clamped to the range's max.
        fn clamped(app: &mut App, target: Entity) -> f32 {
            let node = *app.world().get::<AudioNode>(target).unwrap();
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.set_param(node, UnitParam::Drive, 100.0);
            let mut out = [0.0f32; 2];
            for _ in 0..tutti_graph::PARAM_DECLICK.get() + 64 {
                graph.render_frame(&mut out);
            }
            out[0]
        }

        let at_10 = clamped(&mut app, target);
        assert!(
            (at_10 - (0.5f32 * 10.0).tanh()).abs() < 1e-5,
            "the modulation starts clamped to its declared max; got {at_10}"
        );

        let lfo_before = lfo_node(&app, lfo);
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

        let at_2 = clamped(&mut app, target);
        assert!(
            (at_2 - (0.5f32 * 2.0).tanh()).abs() < 1e-5,
            "the narrowed range must reach the live clamp; got {at_2}, still near \
             tanh(5) means the declaration moved and the render did not"
        );
        assert_eq!(
            lfo_node(&app, lfo),
            lfo_before,
            "the clamp must move in place — respawning the source would restart \
             its phase"
        );
    }
}

/// The two tiers agree: audio-rate modulation sounds like the frame-rate path.
///
/// The matrix delivers a native-param route either per frame (an `AtomicTarget`
/// mirroring into the node's atomic) or per sample (the graph's fused param
/// step, summing each route's shaped source onto the node's control). A route
/// can switch tiers, so the two must compute the same value — a divergence
/// would be heard as a level or timbre jump on a change that is supposed to be
/// inaudible.
///
/// Both tests here are **numeric**: they compare what the graph computes
/// against the `tutti_mod` function the frame-rate accumulator uses, with no
/// `App` and no reconciler. What the *reconciler* declares is the module
/// above's subject; this one is only about the arithmetic at each end.
/// (Was `tests/mod_tier_parity.rs`.)
mod mod_tier_parity {
    use bevy_ecs::prelude::*;

    use bevy_tutti::modulation::ModRoute;
    use tutti_graph::{
        Cx, GraphBuilder, Io, Node, ParamFrom, ParamIn, ParamInput, ParamRange, Prepare, Shape,
        Status, PARAM_DECLICK,
    };
    use tutti_mod::{shape, CurveType, Polarity};
    use tutti_nodes::testing::Const;
    use tutti_nodes::ParamModShaping;
    use tutti_types::graph::OutPort;
    use tutti_types::{ChannelLayout, Depth, ParamAddr, SampleRate, Samples, UnitParam};

    /// A route's shaping, as the graph applies it, agrees with the route's
    /// control-rate shaping.
    ///
    /// Two claims in one, and the second is what can fail at runtime:
    ///
    /// - Every input `ParamModShaping` takes is a field already on `ModRoute`,
    ///   so the translation needs no new authoring vocabulary. (That half is a
    ///   compile-time fact, and the reconciler's own source depends on it.)
    /// - The graph's per-sample shaping matches `tutti_mod::shape` — the exact
    ///   function the frame-rate accumulator applies to the same route. The
    ///   graph bakes depth, polarity and curve into a LUT, so this is a real
    ///   comparison between an interpolated table and the closed form, not a
    ///   tautology.
    ///
    /// `Exponential` rather than `Linear`: a linear curve would agree with almost
    /// any LUT, so the curved case is the one that discriminates.
    #[test]
    fn a_routes_graph_shaping_agrees_with_its_control_rate_shaping() {
        let mut world = World::new();
        let (src, dst) = (world.spawn_empty().id(), world.spawn_empty().id());

        let route = ModRoute::new(src, dst, ParamAddr::Unit(UnitParam::Drive))
            .with_depth(Depth(0.5))
            .with_polarity(Polarity::Unipolar)
            .with_curve(CurveType::Exponential);

        // The shaping is built straight from the route's fields.
        let shaping = ParamModShaping {
            depth: route.depth,
            polarity: route.polarity,
            curve: route.curve,
        }
        .shaping();

        // ...and it agrees with the control-rate shaping of the same route, which
        // is what keeps a route's sound stable if delivery ever switches tiers.
        for x in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
            let got = shaping.apply(x);
            let want = shape(x, route.depth, route.polarity, route.curve);
            assert!(
                (got - want).abs() < 1e-3,
                "audio-rate shaping diverged from the route's control-rate shaping \
                 at {x}: {got} vs {want}"
            );
        }
    }

    /// Declares `Drive`, base 5.0; writes what it reads for it.
    struct Echo;

    impl Node for Echo {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_params(&[UnitParam::Drive])
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            match io.param(0) {
                ParamInput::Base => io.output(0).fill(5.0),
                ParamInput::Frames(v) => io.output(0).copy_from_slice(v),
            }
            Status::Modified
        }
        fn reset(&mut self) {}
        fn param_base(&self, port: usize) -> Option<f32> {
            (port == 0).then_some(5.0)
        }
    }

    /// The arithmetic the graph performs, verified against the same `fold` the
    /// frame-rate accumulator uses. Two full-scale sources at depth 0.25 land the
    /// same value whichever tier evaluates them. Rendered through the graph.
    ///
    /// Mutation (run): in `ParamState::port`, drop the clamp → still 5.5
    /// here (in range), so the clamped case below fails instead: with the
    /// range narrowed to 0..=5.25, the fold gives 5.25.
    #[test]
    fn the_graph_sums_to_what_the_frame_rate_path_would() {
        let depth = Depth(0.25);
        let shaping = ParamModShaping {
            depth,
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        };
        let render = |min: f32, max: f32| -> f32 {
            let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
            let n = g.add(Echo);
            let a = g.add_unit(Box::new(Const::mono(1.0)));
            let b = g.add_unit(Box::new(Const::mono(1.0)));
            let at = ParamIn {
                node: n,
                param: UnitParam::Drive,
            };
            for s in [a, b] {
                g.spec_mut().connect_param(
                    at,
                    ParamFrom::Audio(OutPort { node: s, port: 0 }),
                    shaping.shaping(),
                );
            }
            g.spec_mut().set_param_range(at, ParamRange::new(min, max));
            g.connect_output(n, 0, 0);
            let mut r = g
                .renderer(Prepare::new(SampleRate(48_000.0), Samples(64)))
                .expect("builds");
            *r.render(PARAM_DECLICK.get() + 64)[0]
                .last()
                .expect("rendered")
        };

        let shaped = shape(1.0, depth, Polarity::Bipolar, CurveType::Linear);
        for (min, max) in [(0.0f32, 10.0f32), (0.0, 5.25)] {
            let got = render(min, max);
            let want = tutti_mod::fold(5.0, [shaped, shaped].into_iter(), min, max);
            assert!(
                (got - want).abs() < 1e-6,
                "audio-rate sum {got} disagrees with the control-rate fold {want} \
                 (range {min}..={max})"
            );
        }
    }
}

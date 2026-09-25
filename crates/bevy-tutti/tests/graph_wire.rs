//! Declared wiring reaches the graph — audio ports and param ports alike.
//!
//! - `graph_wire` — wiring declared in the ECS reaches the graph, and stops
//!   reaching it when the declaration goes away. Includes the master root and
//!   its width.
//! - `param_port_wire` — the same declaration mechanism (`PortSources`) applied
//!   to a node's audio-rate **param** ports rather than its signal inputs.
//!
//! One file because they are one mechanism: `PortSources` keys on the sink port
//! index and does not care whether that index names audio or a param, so a
//! rebuild change touches both and only these two suites together show it.

#[macro_use]
mod common;

/// Wiring declared in the ECS reaches the graph, and stops reaching it when the
/// declaration goes away.
///
/// Every assertion reads the engine back through `Net::source` /
/// `Net::output_source` rather than trusting the component, because the diff
/// this layer performs is only meaningful if the engine is the thing being
/// compared against.
/// (Was `tests/graph_wire.rs`.)
mod graph_wire {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;
    use bevy_tutti::graph::GraphBackend;

    use bevy_tutti::graph::{
        AudioGraphRes, GraphDirty, GraphReconcilePlugin, GraphReconcileSystems, MasterSources,
        PortSource, PortSources,
    };
    use bevy_tutti::AudioEngineState;
    // `outputs()` on `Net` is an `AudioUnit` method — the graph's own arity.
    use bevy_tutti::graph::GraphSource;
    use tutti_core::AudioNode;
    use tutti_core::{ChannelLayout, Hz};
    use tutti_nodes::testing::{Osc, Through};

    /// An app wired the way `build_into` leaves one, minus the audio device.
    fn app(backend: GraphBackend) -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless_with(backend, 0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(GraphReconcilePlugin);
        app
    }

    /// Add a node to the graph and bind an entity to it.
    fn spawn_node<U: tutti_core::AudioUnit + 'static>(app: &mut App, unit: U) -> Entity {
        let id = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.insert(unit)
        };
        app.world_mut().spawn(id).id()
    }

    fn node_id(app: &App, entity: Entity) -> AudioNode {
        *app.world().get::<AudioNode>(entity).expect("AudioNode")
    }

    /// The headline claim: a declaration on the sink reaches the engine.
    fn a_declared_source_reaches_the_graph(backend: GraphBackend) {
        let mut app = app(backend);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let filt = spawn_node(&mut app, Through::mono());

        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::from(osc));
        app.update();

        let (osc_id, filt_id) = (node_id(&app, osc), node_id(&app, filt));
        assert_eq!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Node(osc_id, 0)
        );
    }
    both_backends!(a_declared_source_reaches_the_graph);

    /// The master bus is one declaration with one value per channel, so two nodes
    /// cannot both claim it.
    ///
    /// This is the inverse of `master_bus.rs`'s
    /// `a_second_pipe_output_silently_replaces_the_first`: there, the second caller
    /// silently won. Here there is no second caller to have — a resource holds one
    /// value, and a channel holds one source.
    fn the_master_bus_has_one_declaration_not_a_race(backend: GraphBackend) {
        let mut app = app(backend);
        let a = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let b = spawn_node(&mut app, Osc::sine(Hz(880.0)));

        // Both nodes exist and both want the master. Only a declaration decides.
        app.world_mut()
            .insert_resource(MasterSources::from(a).with(1, PortSource::node(b)));
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(
            graph.output_source(0),
            GraphSource::Node(node_id(&app, a), 0)
        );
        assert_eq!(
            graph.output_source(1),
            GraphSource::Node(node_id(&app, b), 0)
        );
    }
    both_backends!(the_master_bus_has_one_declaration_not_a_race);

    /// Removing the declaration silences the ports it claimed.
    ///
    /// The case that is unsolvable imperatively without every call site remembering
    /// what it wired: the entity leaves the rebuild's query, so only the removal
    /// observer can zero those ports.
    fn removing_the_declaration_silences_the_ports(backend: GraphBackend) {
        let mut app = app(backend);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let filt = spawn_node(&mut app, Through::mono());
        let filt_id = node_id(&app, filt);

        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::from(osc));
        app.update();
        assert_ne!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Silence
        );

        app.world_mut().entity_mut(filt).remove::<PortSources>();
        app.update();

        assert_eq!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Silence,
            "a removed declaration must not leave its last wiring behind"
        );
    }
    both_backends!(removing_the_declaration_silences_the_ports);

    /// A declaration naming an entity whose node arrives later is skipped, then
    /// picked up — without anything about the declaration changing.
    ///
    /// This is what `Added<AudioNode>` in the rebuild's dirty gate is for. Gating on
    /// `Changed<PortSources>` alone would leave the wire unformed forever.
    fn an_unresolvable_source_is_skipped_then_picked_up(backend: GraphBackend) {
        let mut app = app(backend);
        let filt = spawn_node(&mut app, Through::mono());
        let filt_id = node_id(&app, filt);

        // An entity with no node yet.
        let pending = app.world_mut().spawn_empty().id();
        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::from(pending));
        app.update();
        assert_eq!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Silence,
            "nothing to resolve yet, and no panic"
        );

        // The node turns up. The declaration is untouched.
        let id = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.insert(Osc::sine(Hz(440.0)))
        };
        app.world_mut().entity_mut(pending).insert(id);
        app.update();

        assert_eq!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Node(id, 0),
            "the wire forms once the node exists — nothing about the declaration \
             changed, so a gate on `Changed<PortSources>` alone would miss it"
        );
    }
    both_backends!(an_unresolvable_source_is_skipped_then_picked_up);

    /// A node naming itself is skipped with a warning, not a panic.
    ///
    /// `Net::set_source` asserts on a self-connection, and an assert inside a
    /// reconcile system takes the app down over a caller's typo.
    fn a_self_connection_is_skipped_not_panicked_on(backend: GraphBackend) {
        let mut app = app(backend);
        let filt = spawn_node(&mut app, Through::mono());

        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::from(filt));
        app.update();

        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .source(node_id(&app, filt), 0),
            GraphSource::Silence
        );
    }
    both_backends!(a_self_connection_is_skipped_not_panicked_on);

    /// A source port past the node's output count is skipped rather than asserting.
    fn an_out_of_range_source_port_is_skipped(backend: GraphBackend) {
        let mut app = app(backend);
        let mono = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let filt = spawn_node(&mut app, Through::mono());

        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::silent().with(
                0,
                PortSource::Node {
                    entity: mono,
                    port: 7,
                },
            ));
        app.update();

        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .source(node_id(&app, filt), 0),
            GraphSource::Silence
        );
    }
    both_backends!(an_out_of_range_source_port_is_skipped);

    /// Re-binding an entity to a different node re-derives every wire naming it.
    ///
    /// This is the whole reason `PortSource::Node` holds an `Entity` rather than a
    /// `NodeId` — and it did not work: the dirty gate was `Added<AudioNode>`, but a
    /// replacement `insert` on an entity that already has the component fires
    /// `Changed` without `Added`. Wires kept pointing at the retired node forever.
    fn re_binding_an_entity_to_a_new_node_re_derives_the_wire(backend: GraphBackend) {
        let mut app = app(backend);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let filt = spawn_node(&mut app, Through::mono());
        let filt_id = node_id(&app, filt);

        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::from(osc));
        app.update();
        let first = node_id(&app, osc);
        assert_eq!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Node(first, 0)
        );

        // Same entity, different node.
        let second = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.insert(Osc::sine(Hz(880.0)))
        };
        app.world_mut().entity_mut(osc).insert(second);
        app.update();

        assert_eq!(
            app.world().resource::<AudioGraphRes>().source(filt_id, 0),
            GraphSource::Node(second, 0),
            "the declaration names an entity, so re-binding that entity must move \
             the wire — otherwise it points at a node nothing renders"
        );
    }
    both_backends!(re_binding_an_entity_to_a_new_node_re_derives_the_wire);

    /// Removing a declaration silences only the ports it claimed.
    ///
    /// The write path clamps to the declared length, so the removal path must too.
    /// Zeroing every input port instead reaches into wiring this layer never made.
    fn removing_a_declaration_leaves_undeclared_ports_alone(backend: GraphBackend) {
        let mut app = app(backend);
        let declared_src = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let foreign_src = spawn_node(&mut app, Osc::sine(Hz(880.0)));
        // Two inputs; the declaration will claim only port 0.
        let sink = spawn_node(&mut app, Through::new(ChannelLayout::STEREO));
        let (sink_id, foreign_id) = (node_id(&app, sink), node_id(&app, foreign_src));

        app.world_mut()
            .entity_mut(sink)
            .insert(PortSources::from(declared_src));
        // Port 1 wired by hand — undeclared, so this layer must not own it.
        {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.set_source(sink_id, 1, GraphSource::Node(foreign_id, 0));
        }
        app.update();

        app.world_mut().entity_mut(sink).remove::<PortSources>();
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(
            graph.source(sink_id, 0),
            GraphSource::Silence,
            "the declared port is released"
        );
        assert_eq!(
            graph.source(sink_id, 1),
            GraphSource::Node(foreign_id, 0),
            "but an undeclared port belongs to whoever wired it"
        );
    }
    both_backends!(removing_a_declaration_leaves_undeclared_ports_alone);

    /// A mono node reaches both master channels — via the constructor that says so.
    ///
    /// `MasterSources::from` names ports 0 and 1, which is correct for a stereo
    /// source and unresolvable for a mono one. Its doc used to promise `pipe_output`'s
    /// modulo wrapping, which it never did: channel 1 was skipped, leaving whatever
    /// the channel previously held still audible.
    fn a_mono_source_can_claim_both_master_channels(backend: GraphBackend) {
        let mut app = app(backend);
        let mono = spawn_node(&mut app, Osc::sine(Hz(440.0)));

        app.world_mut()
            .insert_resource(MasterSources::mono_from(mono));
        app.update();

        let mono_id = node_id(&app, mono);
        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.output_source(0), GraphSource::Node(mono_id, 0));
        assert_eq!(
            graph.output_source(1),
            GraphSource::Node(mono_id, 0),
            "both channels take the mono node's only port"
        );
    }
    both_backends!(a_mono_source_can_claim_both_master_channels);

    /// A stereo master claim replaced by a mono one must not strand the old node.
    ///
    /// The failure this guards: `from` on a mono node leaves channel 1 unresolvable,
    /// the rebuild skips it, and the *previous* master keeps rendering on the right
    /// — two nodes owning the bus, which is the defect the declarative layer exists
    /// to make impossible.
    fn replacing_a_stereo_master_with_a_mono_one_releases_both_channels(backend: GraphBackend) {
        let mut app = app(backend);
        let stereo = spawn_node(&mut app, Through::new(ChannelLayout::STEREO));
        let mono = spawn_node(&mut app, Osc::sine(Hz(440.0)));

        app.world_mut().insert_resource(MasterSources::from(stereo));
        app.update();
        let stereo_id = node_id(&app, stereo);
        assert_eq!(
            app.world().resource::<AudioGraphRes>().output_source(1),
            GraphSource::Node(stereo_id, 1)
        );

        app.world_mut()
            .insert_resource(MasterSources::mono_from(mono));
        app.update();

        let mono_id = node_id(&app, mono);
        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.output_source(0), GraphSource::Node(mono_id, 0));
        assert_ne!(
            graph.output_source(1),
            GraphSource::Node(stereo_id, 1),
            "the retracted node must not still be feeding a channel"
        );
        assert_eq!(graph.output_source(1), GraphSource::Node(mono_id, 0));
    }
    both_backends!(replacing_a_stereo_master_with_a_mono_one_releases_both_channels);

    /// A rebuild that finds the engine already agreeing writes nothing — it does not
    /// re-set ports that already hold the declared source.
    ///
    /// This is what the diff buys. `Net::set_source` calls `invalidate_order()`,
    /// throwing away the cached topological sort, so re-writing an unchanged port is
    /// not free; and marking `GraphDirty` forces a commit the frame did not need.
    ///
    /// Driven by adding a *second, unrelated* node, which dirties the rebuild via
    /// `Added<AudioNode>` without changing any existing declaration. Without the
    /// diff, every already-correct port is rewritten and the frame is dirtied.
    ///
    /// The flag has to be sampled **between** the rebuild and `commit_graph`, which
    /// clears it in the `Commit` set — reading it after `update()` returns shows
    /// `false` whether or not the rebuild wrote, which is how a first version of
    /// this test passed against a deliberately un-diffed rebuild.
    fn a_rebuild_that_changes_nothing_writes_nothing(backend: GraphBackend) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut app = app(backend);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let filt = spawn_node(&mut app, Through::mono());
        app.world_mut()
            .entity_mut(filt)
            .insert(PortSources::from(osc));
        app.update();

        // Sample GraphDirty after the rebuild and before the commit clears it.
        let dirtied = Arc::new(AtomicBool::new(false));
        let probe = dirtied.clone();
        app.add_systems(
            Update,
            (move |dirty: Res<GraphDirty>| {
                if dirty.0 {
                    probe.store(true, Ordering::SeqCst);
                }
            })
            .after(GraphReconcileSystems::Compensate)
            .before(GraphReconcileSystems::Commit),
        );

        // A new node elsewhere: the rebuild runs, but nothing it already wired has
        // changed.
        spawn_node(&mut app, Osc::sine(Hz(880.0)));
        app.update();

        assert!(
            !dirtied.load(Ordering::SeqCst),
            "a rebuild whose declarations all match the engine must not dirty the \
             graph — re-writing a correct port discards the cached node order and \
             forces a commit the frame did not need"
        );
    }
    both_backends!(a_rebuild_that_changes_nothing_writes_nothing);

    // ---------------------------------------------------------------------------
    // A master declaration wider than the root widens the root.
    // ---------------------------------------------------------------------------

    /// A 6-channel `MasterSources` on a stereo root must widen the root, not be
    /// truncated to it.
    ///
    /// Before this, `rebuild` clamped the loop to `graph.outputs()`, so channels
    /// 2-5 were dropped with no warning and nothing in the ECS to inspect. The
    /// second half of this test is what makes it a real regression: a fix that
    /// widens the root but leaves the loop clamped passes the arity assertion and
    /// still never wires channel 5.
    fn a_wider_master_declaration_widens_the_root(backend: GraphBackend) {
        let mut app = app(backend);
        let wide = spawn_node(&mut app, Through::new(ChannelLayout::from(6u16)));

        app.insert_resource(MasterSources::from_node_at_width(
            wide,
            tutti_core::ChannelLayout::from(6u16),
        ));
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.outputs(), 6, "the declaration widened the root");

        let id = node_id(&app, wide);
        for channel in 0..6 {
            assert_eq!(
                graph.output_source(channel),
                GraphSource::Node(id, channel),
                "channel {channel} must actually be wired, not merely reachable"
            );
        }
    }
    both_backends!(a_wider_master_declaration_widens_the_root);

    /// Widening must survive the real commit path. A plain `Net::commit` panics on
    /// an arity change, so this is what proves the arity-permitting commit is
    /// genuinely the one reached.
    fn a_widened_root_survives_a_real_commit(backend: GraphBackend) {
        let mut app = app(backend);
        let wide = spawn_node(&mut app, Through::new(ChannelLayout::from(6u16)));

        app.insert_resource(MasterSources::from_node_at_width(
            wide,
            tutti_core::ChannelLayout::from(6u16),
        ));
        app.update();
        // A second frame: the commit runs in `Commit`, after `rebuild`'s widening.
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.outputs(), 6);
        assert!(
            !app.world().resource::<GraphDirty>().0,
            "the commit consumed the dirty flag rather than panicking on the arity change"
        );
    }
    both_backends!(a_widened_root_survives_a_real_commit);

    /// A *shorter* declaration means undeclared, not "narrow the root".
    ///
    /// Narrowing on a shortened `Vec` would tear down channels the host may own
    /// imperatively — the same violation `unwire_removed_sources` refuses. It needs
    /// its own explicit API, not an inference from a length.
    fn a_shorter_master_declaration_does_not_narrow_the_root(backend: GraphBackend) {
        let mut app = app(backend);
        let wide = spawn_node(&mut app, Through::new(ChannelLayout::from(6u16)));

        app.insert_resource(MasterSources::from_node_at_width(
            wide,
            tutti_core::ChannelLayout::from(6u16),
        ));
        app.update();
        assert_eq!(app.world().resource::<AudioGraphRes>().outputs(), 6);

        // Now declare only two channels.
        app.insert_resource(MasterSources::from(wide));
        app.update();

        assert_eq!(
            app.world().resource::<AudioGraphRes>().outputs(),
            6,
            "a shorter declaration is undeclared, not a narrowing instruction"
        );
    }
    both_backends!(a_shorter_master_declaration_does_not_narrow_the_root);

    /// Channels past `MAX_ROOT_CHANNELS` are refused at the clamp, not silently
    /// dropped one layer down — the render scratch is bounded, so a root wider than
    /// it would report channels that are declarable but never rendered.
    fn a_master_declaration_cannot_exceed_the_render_scratch(backend: GraphBackend) {
        let mut app = app(backend);
        let node = spawn_node(&mut app, Osc::sine(Hz(440.0)));

        let mut sources = MasterSources::default();
        for channel in 0..32 {
            sources = sources.with(channel, PortSource::node(node));
        }
        app.insert_resource(sources);
        app.update();

        assert!(
            app.world().resource::<AudioGraphRes>().outputs() <= 8,
            "the root must stay within the render scratch"
        );
    }
    both_backends!(a_master_declaration_cannot_exceed_the_render_scratch);

    // ---------------------------------------------------------------------------
    // The N-wide constructors.
    // ---------------------------------------------------------------------------

    /// `from_node_at_width` is the identity mapping — every channel straight
    /// through, no wrap, no fold.
    fn from_node_at_width_maps_every_channel_straight_through(backend: GraphBackend) {
        let mut app = app(backend);
        let sink = spawn_node(&mut app, Through::new(ChannelLayout::from(6u16)));
        let src = spawn_node(&mut app, Through::new(ChannelLayout::from(6u16)));

        app.world_mut()
            .entity_mut(sink)
            .insert(PortSources::from_node_at_width(
                src,
                tutti_core::ChannelLayout::from(6u16),
            ));
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        let (sink_id, src_id) = (node_id(&app, sink), node_id(&app, src));
        for port in 0..6 {
            assert_eq!(
                graph.source(sink_id, port),
                GraphSource::Node(src_id, port),
                "port {port} must come from the matching source port"
            );
        }
    }
    both_backends!(from_node_at_width_maps_every_channel_straight_through);

    /// At stereo it is exactly `stereo_from`. That equivalence is what lets a
    /// reviewer trust every existing call site is unaffected by the new
    /// constructor's arrival.
    fn from_node_at_width_at_stereo_is_stereo_from(backend: GraphBackend) {
        let mut app = app(backend);
        let e = spawn_node(&mut app, Through::mono());

        assert_eq!(
            PortSources::from_node_at_width(e, tutti_core::ChannelLayout::STEREO),
            PortSources::stereo_from(e)
        );
    }
    both_backends!(from_node_at_width_at_stereo_is_stereo_from);
}

/// Spike: declaring an audio-rate **param** port through `PortSources`.
///
/// The imperative form of this edge is fragile — `Net::pipe_input` walks every
/// input port of a node, so a later "wire the audio in" call silently
/// overwrites a param edge (see tutti-nodes' `audio_rate_param_mod` test). This
/// file asks whether routing the same edge through the declarative layer makes
/// that clobber unrepresentable.
/// (Was `tests/param_port_wire.rs`.)
mod param_port_wire {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;
    use bevy_tutti::graph::GraphBackend;

    use bevy_tutti::graph::GraphSource;
    use bevy_tutti::graph::{
        AudioGraphRes, GraphReconcilePlugin, MasterSources, PortSource, PortSources,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::AudioNode;
    use tutti_core::Hz;
    use tutti_nodes::testing::Osc;
    use tutti_nodes::{AtomicSourceNode, DistortionNode, ParamPorts, ParamSumNode, ShapeKind};
    use tutti_types::UnitParam;

    fn app(backend: GraphBackend) -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless_with(backend, 0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(GraphReconcilePlugin);
        app
    }

    fn spawn_node<U: tutti_core::AudioUnit + 'static>(app: &mut App, unit: U) -> Entity {
        let id = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.insert(unit)
        };
        app.world_mut().spawn(id).id()
    }

    fn node_id(app: &App, entity: Entity) -> AudioNode {
        *app.world().get::<AudioNode>(entity).expect("AudioNode")
    }

    /// The headline: audio and param ports declared on **one** component, both
    /// reaching the engine.
    ///
    /// This is what makes the port space have a single writer — the thing the
    /// imperative form cannot guarantee.
    fn audio_and_param_ports_are_declared_together(backend: GraphBackend) {
        let mut app = app(backend);

        // A distortion born with its drive port on: inputs are [L, R, drive].
        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
        let drive_port = dist.param_port(UnitParam::Drive).expect("drive port");
        assert_eq!(drive_port, 2, "the param port follows the audio inputs");

        let target = spawn_node(&mut app, dist);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        // The base-sum chain feeding the param port.
        let base = spawn_node(&mut app, AtomicSourceNode::new(9.0));
        let sum = spawn_node(&mut app, ParamSumNode::new(0, 0.0, 10.0));

        // ONE declaration covering both kinds of port.
        app.world_mut()
            .entity_mut(sum)
            .insert(PortSources::from(base));
        app.world_mut().entity_mut(target).insert(
            PortSources::silent()
                .with(
                    0,
                    PortSource::Node {
                        entity: osc,
                        port: 0,
                    },
                )
                .with(
                    1,
                    PortSource::Node {
                        entity: osc,
                        port: 0,
                    },
                )
                .with(
                    drive_port,
                    PortSource::Node {
                        entity: sum,
                        port: 0,
                    },
                ),
        );
        app.update();

        let (target_id, osc_id, sum_id, base_id) = (
            node_id(&app, target),
            node_id(&app, osc),
            node_id(&app, sum),
            node_id(&app, base),
        );
        let graph = app.world().resource::<AudioGraphRes>();

        assert_eq!(graph.source(target_id, 0), GraphSource::Node(osc_id, 0));
        assert_eq!(graph.source(target_id, 1), GraphSource::Node(osc_id, 0));
        assert_eq!(
            graph.source(target_id, drive_port),
            GraphSource::Node(sum_id, 0),
            "the param port is fed by the sum, declared alongside the audio"
        );
        assert_eq!(graph.source(sum_id, 0), GraphSource::Node(base_id, 0));
    }
    both_backends!(audio_and_param_ports_are_declared_together);

    /// The clobber the imperative form suffers cannot be expressed here.
    ///
    /// `PortSources` is one component per entity (the ECS enforces that), and
    /// `rebuild` writes the whole declared port range from that one `Vec`. So
    /// "something else overwrote the param port" has no representation: re-declaring
    /// the audio ports means editing the same `Vec` that holds the param port, and
    /// a `Vec` shorter than the param index leaves it *undeclared* — untouched, not
    /// zeroed.
    fn redeclaring_audio_does_not_disturb_the_param_port(backend: GraphBackend) {
        let mut app = app(backend);

        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
        let drive_port = dist.param_port(UnitParam::Drive).unwrap();
        let target = spawn_node(&mut app, dist);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let other = spawn_node(&mut app, Osc::sine(Hz(220.0)));
        let sum = spawn_node(&mut app, ParamSumNode::new(0, 0.0, 10.0));

        app.world_mut().entity_mut(target).insert(
            PortSources::silent()
                .with(
                    0,
                    PortSource::Node {
                        entity: osc,
                        port: 0,
                    },
                )
                .with(
                    drive_port,
                    PortSource::Node {
                        entity: sum,
                        port: 0,
                    },
                ),
        );
        app.update();

        let (target_id, sum_id) = (node_id(&app, target), node_id(&app, sum));
        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .source(target_id, drive_port),
            GraphSource::Node(sum_id, 0)
        );

        // Now re-point the AUDIO input — the operation that, imperatively, would
        // have been `pipe_input` and would have taken the param edge with it.
        let mut decl = app.world_mut().get_mut::<PortSources>(target).unwrap();
        decl.0[0] = PortSource::Node {
            entity: other,
            port: 0,
        };
        app.update();

        let other_id = node_id(&app, other);
        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(
            graph.source(target_id, 0),
            GraphSource::Node(other_id, 0),
            "the audio input moved"
        );
        assert_eq!(
            graph.source(target_id, drive_port),
            GraphSource::Node(sum_id, 0),
            "and the param edge is untouched — one writer owns the whole port space"
        );
    }
    both_backends!(redeclaring_audio_does_not_disturb_the_param_port);

    /// A param port the declaration does not mention is left alone, exactly as an
    /// unmentioned audio port is. "Undeclared" and "declared silent" stay distinct.
    fn an_undeclared_param_port_is_untouched(backend: GraphBackend) {
        let mut app = app(backend);

        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
        let drive_port = dist.param_port(UnitParam::Drive).unwrap();
        let target = spawn_node(&mut app, dist);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let sum = spawn_node(&mut app, ParamSumNode::new(0, 0.0, 10.0));

        // Wire the param port imperatively first — a host that has not adopted the
        // declaration for it yet.
        let (target_id, sum_id) = (node_id(&app, target), node_id(&app, sum));
        {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.set_source(target_id, drive_port, GraphSource::Node(sum_id, 0));
        }

        // Declare ONLY the audio ports. The Vec stops before the param index.
        app.world_mut()
            .entity_mut(target)
            .insert(PortSources::from(osc));
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(
            graph.source(target_id, drive_port),
            GraphSource::Node(sum_id, 0),
            "a short declaration leaves trailing ports undeclared, not silenced"
        );
    }
    both_backends!(an_undeclared_param_port_is_untouched);

    /// The full chain, declared: global input → node audio, and
    /// `base → sum → node.drive_port` for the modulation.
    ///
    /// Structure only, matching this crate's other wiring tests — that the chain
    /// *renders* (drive 9.0 saturating where 1.0 does not) is asserted in
    /// tutti-nodes' `audio_rate_param_mod`, where a backend-free `Net` can be ticked
    /// directly. A net with a backend defers to `commit`, so ticking the frontend
    /// here would prove nothing about what the engine runs.
    fn the_whole_declared_chain_reaches_the_graph(backend: GraphBackend) {
        let mut app = app(backend);
        app.insert_resource(MasterSources::default());

        let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 1.0, true);
        let drive_port = dist.param_port(UnitParam::Drive).unwrap();
        let target = spawn_node(&mut app, dist);
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let base = spawn_node(&mut app, AtomicSourceNode::new(9.0));
        let sum = spawn_node(&mut app, ParamSumNode::new(0, 0.0, 10.0));

        app.world_mut()
            .entity_mut(sum)
            .insert(PortSources::from(base));
        app.world_mut().entity_mut(target).insert(
            PortSources::silent()
                .with(
                    0,
                    PortSource::Node {
                        entity: osc,
                        port: 0,
                    },
                )
                .with(
                    1,
                    PortSource::Node {
                        entity: osc,
                        port: 0,
                    },
                )
                .with(
                    drive_port,
                    PortSource::Node {
                        entity: sum,
                        port: 0,
                    },
                ),
        );
        app.update();

        let (target_id, osc_id, sum_id, base_id) = (
            node_id(&app, target),
            node_id(&app, osc),
            node_id(&app, sum),
            node_id(&app, base),
        );
        let graph = app.world().resource::<AudioGraphRes>();

        // Audio in from the oscillator...
        assert_eq!(graph.source(target_id, 0), GraphSource::Node(osc_id, 0));
        assert_eq!(graph.source(target_id, 1), GraphSource::Node(osc_id, 0));
        // ...and the modulation chain into the param port, all from one declaration.
        assert_eq!(
            graph.source(target_id, drive_port),
            GraphSource::Node(sum_id, 0)
        );
        assert_eq!(graph.source(sum_id, 0), GraphSource::Node(base_id, 0));
    }
    both_backends!(the_whole_declared_chain_reaches_the_graph);
}

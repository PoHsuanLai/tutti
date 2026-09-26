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
/// Every assertion reads the engine back through `AudioGraphRes::source` /
/// `AudioGraphRes::output_source` rather than trusting the component, because the diff
/// this layer performs is only meaningful if the engine is the thing being
/// compared against.
/// (Was `tests/graph_wire.rs`.)
mod graph_wire {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{
        AudioGraphRes, GraphDirty, GraphReconcilePlugin, GraphReconcileSystems, MasterSources,
        PortSource, PortSources,
    };
    use bevy_tutti::AudioEngineState;
    // `GraphSource` is the graph's own spelling of what feeds a port.
    use bevy_tutti::graph::GraphSource;
    use tutti_core::AudioNode;
    use tutti_core::{ChannelLayout, Hz};
    use tutti_nodes::testing::{Osc, Through};

    /// An app wired the way `build_into` leaves one, minus the audio device.
    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
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
    #[test]
    fn a_declared_source_reaches_the_graph() {
        let mut app = app();
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

    /// The master bus is one declaration with one value per channel, so two nodes
    /// cannot both claim it.
    ///
    /// This is the inverse of `master_bus.rs`'s
    /// `a_second_pipe_output_silently_replaces_the_first`: there, the second caller
    /// silently won. Here there is no second caller to have — a resource holds one
    /// value, and a channel holds one source.
    #[test]
    fn the_master_bus_has_one_declaration_not_a_race() {
        let mut app = app();
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

    /// Removing the declaration silences the ports it claimed.
    ///
    /// The case that is unsolvable imperatively without every call site remembering
    /// what it wired: the entity leaves the rebuild's query, so only the removal
    /// observer can zero those ports.
    #[test]
    fn removing_the_declaration_silences_the_ports() {
        let mut app = app();
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

    /// A declaration naming an entity whose node arrives later is skipped, then
    /// picked up — without anything about the declaration changing.
    ///
    /// This is what `Added<AudioNode>` in the rebuild's dirty gate is for. Gating on
    /// `Changed<PortSources>` alone would leave the wire unformed forever.
    #[test]
    fn an_unresolvable_source_is_skipped_then_picked_up() {
        let mut app = app();
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

    /// A node naming itself is skipped with a warning, not a panic.
    ///
    /// `Net::set_source` asserts on a self-connection, and an assert inside a
    /// reconcile system takes the app down over a caller's typo.
    #[test]
    fn a_self_connection_is_skipped_not_panicked_on() {
        let mut app = app();
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

    /// A source port past the node's output count is skipped rather than asserting.
    #[test]
    fn an_out_of_range_source_port_is_skipped() {
        let mut app = app();
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

    /// Re-binding an entity to a different node re-derives every wire naming it.
    ///
    /// This is the whole reason `PortSource::Node` holds an `Entity` rather than a
    /// `NodeId` — and it did not work: the dirty gate was `Added<AudioNode>`, but a
    /// replacement `insert` on an entity that already has the component fires
    /// `Changed` without `Added`. Wires kept pointing at the retired node forever.
    #[test]
    fn re_binding_an_entity_to_a_new_node_re_derives_the_wire() {
        let mut app = app();
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

    /// Removing a declaration silences only the ports it claimed.
    ///
    /// The write path clamps to the declared length, so the removal path must too.
    /// Zeroing every input port instead reaches into wiring this layer never made.
    #[test]
    fn removing_a_declaration_leaves_undeclared_ports_alone() {
        let mut app = app();
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

    /// A lookahead limiter: a node that reports latency, so a path through it
    /// moves the graph's PDC plan.
    fn latent() -> tutti_nodes::LimiterNode {
        tutti_nodes::LimiterNode::with_channels(
            ChannelLayout::MONO,
            tutti_core::Db(-6.0),
            tutti_core::Db(-1.0),
        )
    }

    /// **A host that wires the master itself, through a latent node, is left
    /// alone — and the rebuild's debug check does not panic over it.** An
    /// empty `MasterSources` declares no output channel, so the value carries
    /// none; the host wires output 0 from a limiter by hand. The rebuild's
    /// consistency check used to compare the latency plan of the *whole*
    /// graph against the value's, and the value — which has no outputs —
    /// folds to no latency where the graph has the limiter's: a debug build
    /// panicked over a graph that was right. The check covers only what the
    /// value declares now.
    ///
    /// Mutation (run): `topology::disagreements` comparing
    /// `latency::plan(want)` against `graph.latency_plan()` again → this
    /// panics in the rebuild ("the engine does not match the value").
    #[test]
    fn a_hand_wired_master_behind_a_latent_node_is_not_a_disagreement() {
        let mut app = app();
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let lim = spawn_node(&mut app, latent());
        let (osc_id, lim_id) = (node_id(&app, osc), node_id(&app, lim));
        // Declared: the limiter's input, so the rebuild runs this frame.
        app.world_mut()
            .entity_mut(lim)
            .insert(PortSources::from(osc));
        // Undeclared: the master, wired by hand — through the limiter on 0,
        // dry on 1, so PDC has something to align.
        {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            assert!(
                graph.node_latency(lim_id).get() > 0,
                "the limiter is latent"
            );
            graph.set_output_source(0, GraphSource::Node(lim_id, 0));
            graph.set_output_source(1, GraphSource::Node(osc_id, 0));
        }
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.source(lim_id, 0), GraphSource::Node(osc_id, 0));
        assert_eq!(
            graph.output_source(0),
            GraphSource::Node(lim_id, 0),
            "the host's master is its own"
        );
        assert_eq!(graph.output_source(1), GraphSource::Node(osc_id, 0));
        assert_eq!(
            graph.latency_plan().total(),
            graph.node_latency(lim_id),
            "and the graph compensates the path the host wired"
        );
    }

    /// **A port a short `PortSources` leaves undeclared, wired by hand from a
    /// latent node, is not a disagreement either.** The sink declares port 0
    /// and the master declares the sink; the host wires the sink's port 1
    /// from a limiter. The value has no edge at port 1, so it folds to no
    /// latency on channel 1 where the graph has the limiter's — the second
    /// way the whole-graph plan comparison panicked a debug build.
    ///
    /// Mutation (run): the whole-graph plan comparison restored in
    /// `topology::disagreements` → this panics in the rebuild.
    #[test]
    fn a_hand_wired_port_behind_a_latent_node_is_not_a_disagreement() {
        let mut app = app();
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let lim = spawn_node(&mut app, latent());
        let sink = spawn_node(&mut app, Through::new(ChannelLayout::STEREO));
        let (osc_id, lim_id, sink_id) =
            (node_id(&app, osc), node_id(&app, lim), node_id(&app, sink));
        app.world_mut()
            .entity_mut(lim)
            .insert(PortSources::from(osc));
        // Port 0 declared; port 1 left to whoever wires it.
        app.world_mut()
            .entity_mut(sink)
            .insert(PortSources::from(osc));
        app.world_mut().insert_resource(MasterSources::from(sink));
        {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.set_source(sink_id, 1, GraphSource::Node(lim_id, 0));
        }
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.source(sink_id, 0), GraphSource::Node(osc_id, 0));
        assert_eq!(
            graph.source(sink_id, 1),
            GraphSource::Node(lim_id, 0),
            "the undeclared port is the host's"
        );
        assert_eq!(
            graph.latency_plan().total(),
            graph.node_latency(lim_id),
            "and the graph compensates the path the host wired"
        );
    }

    /// A mono node reaches both master channels — via the constructor that says so.
    ///
    /// `MasterSources::from` names ports 0 and 1, which is correct for a stereo
    /// source and unresolvable for a mono one. Its doc used to promise `pipe_output`'s
    /// modulo wrapping, which it never did: channel 1 was skipped, leaving whatever
    /// the channel previously held still audible.
    #[test]
    fn a_mono_source_can_claim_both_master_channels() {
        let mut app = app();
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

    /// A stereo master claim replaced by a mono one must not strand the old node.
    ///
    /// The failure this guards: `from` on a mono node leaves channel 1 unresolvable,
    /// the rebuild skips it, and the *previous* master keeps rendering on the right
    /// — two nodes owning the bus, which is the defect the declarative layer exists
    /// to make impossible.
    #[test]
    fn replacing_a_stereo_master_with_a_mono_one_releases_both_channels() {
        let mut app = app();
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
    #[test]
    fn a_rebuild_that_changes_nothing_writes_nothing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut app = app();
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
    #[test]
    fn a_wider_master_declaration_widens_the_root() {
        let mut app = app();
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

    /// Widening must survive the real commit path. A plain `Net::commit` panics on
    /// an arity change, so this is what proves the arity-permitting commit is
    /// genuinely the one reached.
    #[test]
    fn a_widened_root_survives_a_real_commit() {
        let mut app = app();
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

    /// A *shorter* declaration silences the channels it dropped, but does not
    /// narrow the root.
    ///
    /// The root is sized to the device; narrowing it on a shortened `Vec` would
    /// have the engine fold channels the host never asked to lose. Narrowing
    /// needs its own explicit API, not an inference from a length. What the
    /// dropped channels read instead is
    /// `shrinking_the_master_releases_the_dropped_channel`'s claim.
    #[test]
    fn a_shorter_master_declaration_does_not_narrow_the_root() {
        let mut app = app();
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
            "a shorter declaration is not a narrowing instruction"
        );
    }

    /// Shrinking `MasterSources` releases the channel it dropped: the node
    /// that fed it no longer does, the root keeps its width, and the latency
    /// plan is the one over the graph that now exists.
    ///
    /// Channel 1 is fed through a limiter, so it is the channel that defines
    /// the graph's latency. Before the fix the dropped channel kept its
    /// source — nothing declared it any more, so nothing wrote it — and the
    /// value, one channel short of the root, folded to a different latency
    /// plan from the engine's (channel 1 still behind the limiter), which
    /// tripped `rebuild`'s "the engine does not match the value" check. (That
    /// check no longer compares whole-graph plans — see
    /// `a_hand_wired_master_behind_a_latent_node_is_not_a_disagreement` — so
    /// what catches a regression now is this test's own reading of channel 1.)
    ///
    /// Mutation (run): building the value's outputs to the declaration's
    /// length, as before (`0..graph.outputs().min(master.0.len())`) → channel
    /// 1 keeps the limiter → "the dropped channel's source is disconnected"
    /// fails.
    #[test]
    fn shrinking_the_master_releases_the_dropped_channel() {
        use tutti_core::Db;
        use tutti_nodes::LimiterNode;

        let mut app = app();
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let lim = spawn_node(
            &mut app,
            LimiterNode::with_channels(ChannelLayout::MONO, Db(0.0), Db(0.0)),
        );
        app.world_mut()
            .entity_mut(lim)
            .insert(PortSources::from(osc));
        app.insert_resource(
            MasterSources::default()
                .with(0, PortSource::node(osc))
                .with(1, PortSource::node(lim)),
        );
        app.update();

        let lim_id = node_id(&app, lim);
        let latency = {
            let graph = app.world().resource::<AudioGraphRes>();
            assert_eq!(graph.output_source(1), GraphSource::Node(lim_id, 0));
            let lat = graph.node_latency(lim_id);
            assert!(lat.get() > 0, "the limiter must report its lookahead");
            assert_eq!(
                graph.latency_plan().total(),
                lat,
                "channel 1 is the slow path"
            );
            lat
        };

        // The shrink: channel 1 is no longer declared, while `lim` is still
        // in the graph and still wired from `osc`.
        app.insert_resource(MasterSources::default().with(0, PortSource::node(osc)));
        app.update();

        let osc_id = node_id(&app, osc);
        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.output_source(0), GraphSource::Node(osc_id, 0));
        assert_eq!(
            graph.output_source(1),
            GraphSource::Silence,
            "the dropped channel's source is disconnected"
        );
        assert_eq!(
            graph.outputs(),
            2,
            "the root keeps its width; a shrink is not a narrowing"
        );
        assert_eq!(
            graph.source(lim_id, 0),
            GraphSource::Node(osc_id, 0),
            "the limiter's own input is still declared, so it is untouched"
        );
        let plan = graph.latency_plan();
        assert_eq!(
            plan.total(),
            tutti_core::Samples(0),
            "no output reaches the limiter any more (it was {latency:?})"
        );
        assert_eq!(plan.channels().len(), 2, "one figure per root channel");

        // And the value agrees: `LiveGraph` describes the whole root, so the
        // same fold over it gives the same figures.
        let live = app.world().resource::<bevy_tutti::graph::LiveGraph>();
        assert_eq!(live.topology().outputs.len(), 2);
        assert_eq!(tutti_types::latency::plan(live.topology()), plan);
    }

    /// Channels past `MAX_ROOT_CHANNELS` are refused at the clamp, not silently
    /// dropped one layer down — the render scratch is bounded, so a root wider than
    /// it would report channels that are declarable but never rendered.
    #[test]
    fn a_master_declaration_cannot_exceed_the_render_scratch() {
        let mut app = app();
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

    // ---------------------------------------------------------------------------
    // The N-wide constructors.
    // ---------------------------------------------------------------------------

    /// `from_node_at_width` is the identity mapping — every channel straight
    /// through, no wrap, no fold.
    #[test]
    fn from_node_at_width_maps_every_channel_straight_through() {
        let mut app = app();
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

    /// At stereo it is exactly `stereo_from`. That equivalence is what lets a
    /// reviewer trust every existing call site is unaffected by the new
    /// constructor's arrival.
    #[test]
    fn from_node_at_width_at_stereo_is_stereo_from() {
        let mut app = app();
        let e = spawn_node(&mut app, Through::mono());

        assert_eq!(
            PortSources::from_node_at_width(e, tutti_core::ChannelLayout::STEREO),
            PortSources::stereo_from(e)
        );
    }
}

/// A param modulation and a node's audio ports are two spaces: declaring one
/// cannot clobber the other.
///
/// This module used to ask whether routing an audio-rate *param port* — an
/// extra input channel after a node's audio inputs, fed by a base/sum chain —
/// through `PortSources` made the clobber an imperative `pipe_input` suffered
/// unrepresentable. The graph now modulates params through its own param
/// edges (design doc 013 item 6), not input channels, so the clobber has no
/// representation at all; these pin that the two spaces stay apart through
/// the declarative layer. (Was `tests/param_port_wire.rs`.)
mod param_mod_wire {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::GraphSource;
    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, PortSource, PortSources};
    use bevy_tutti::AudioEngineState;
    use tutti_core::AudioNode;
    use tutti_core::Hz;
    use tutti_graph::{ParamRange, ParamShaping};
    use tutti_nodes::testing::Osc;
    use tutti_nodes::{DistortionNode, ShapeKind};
    use tutti_types::UnitParam;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
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

    /// Audio ports declared, and the drive modulated: both reach the graph,
    /// and the node's arity is its audio width — the param is no port.
    #[test]
    fn audio_ports_and_a_param_modulation_reach_the_graph_together() {
        let mut app = app();
        let target = spawn_node(&mut app, DistortionNode::new(ShapeKind::Tanh, 5.0));
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let lfo = spawn_node(&mut app, Osc::sine(Hz(2.0)));
        let (target_id, osc_id, lfo_id) = (
            node_id(&app, target),
            node_id(&app, osc),
            node_id(&app, lfo),
        );

        let o = PortSource::Node {
            entity: osc,
            port: 0,
        };
        app.world_mut()
            .entity_mut(target)
            .insert(PortSources::silent().with(0, o).with(1, o));
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_param_mod(
                target_id,
                UnitParam::Drive,
                &[(lfo_id, ParamShaping::Identity)],
                ParamRange::new(0.0, 10.0),
            );
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(graph.node_inputs(target_id), 2, "no param port: two inputs");
        assert!(graph.declares_param(target_id, UnitParam::Drive));
        assert_eq!(graph.source(target_id, 0), GraphSource::Node(osc_id, 0));
        assert_eq!(graph.source(target_id, 1), GraphSource::Node(osc_id, 0));
        assert_eq!(
            graph
                .param_mod(target_id, UnitParam::Drive)
                .expect("the modulation")
                .sources
                .len(),
            1,
            "the drive is modulated beside the audio"
        );
    }

    /// Re-declaring the audio ports — the operation that, imperatively, would
    /// have been `pipe_input` and would have taken a param edge with it —
    /// leaves the modulation untouched, and clearing the modulation leaves
    /// the audio untouched.
    ///
    /// Mutation (run): make the wire rebuild clear `spec.params` for a node
    /// whose audio it rewrites → the first assertion after the redeclaration
    /// fails.
    #[test]
    fn redeclaring_audio_does_not_disturb_the_param_modulation() {
        let mut app = app();
        let target = spawn_node(&mut app, DistortionNode::new(ShapeKind::Tanh, 5.0));
        let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
        let other = spawn_node(&mut app, Osc::sine(Hz(220.0)));
        let lfo = spawn_node(&mut app, Osc::sine(Hz(2.0)));
        let (target_id, lfo_id) = (node_id(&app, target), node_id(&app, lfo));

        app.world_mut()
            .entity_mut(target)
            .insert(PortSources::silent().with(
                0,
                PortSource::Node {
                    entity: osc,
                    port: 0,
                },
            ));
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_param_mod(
                target_id,
                UnitParam::Drive,
                &[(lfo_id, ParamShaping::Identity)],
                ParamRange::new(0.0, 10.0),
            );
        app.update();
        let before = app
            .world()
            .resource::<AudioGraphRes>()
            .param_mod(target_id, UnitParam::Drive)
            .expect("the modulation");

        let mut decl = app.world_mut().get_mut::<PortSources>(target).unwrap();
        decl.0[0] = PortSource::Node {
            entity: other,
            port: 0,
        };
        app.update();

        let other_id = node_id(&app, other);
        {
            let graph = app.world().resource::<AudioGraphRes>();
            assert_eq!(
                graph.param_mod(target_id, UnitParam::Drive),
                Some(before),
                "the modulation is untouched by an audio redeclaration"
            );
            assert_eq!(
                graph.source(target_id, 0),
                GraphSource::Node(other_id, 0),
                "the audio input moved"
            );
        }

        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .clear_param_mod(target_id, UnitParam::Drive);
        app.update();
        let graph = app.world().resource::<AudioGraphRes>();
        assert!(graph.param_mod(target_id, UnitParam::Drive).is_none());
        assert_eq!(
            graph.source(target_id, 0),
            GraphSource::Node(other_id, 0),
            "and clearing the modulation leaves the audio alone"
        );
    }
}

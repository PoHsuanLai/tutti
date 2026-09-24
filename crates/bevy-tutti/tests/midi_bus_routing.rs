//! The software MIDI bus: who is on it, where events go, and what takes them off.
//!
//! One module per scenario, all three about the same seam — an entity's MIDI
//! identity and the routing snapshot the audio thread reads:
//!
//! - `midi_route` — a route declared in the ECS reaches that snapshot.
//! - `midi_registration` — a node's sender joins the bus, and leaves it again.
//! - `plugin_crash_unwire` — losing `AudioNode` takes the entity off both the
//!   graph and the bus, which is the removal half of `midi_registration`'s pair.
//!
//! Grouped because all three key on the same component (`AudioNode`) and the
//! same registry, and a change to registration is exactly what would break
//! unwiring silently. Bodies and test names are unchanged from the three files
//! these came from; each module keeps its own helpers.

#![cfg(all(feature = "midi", feature = "synth"))]

/// A route declared in the ECS reaches the snapshot the audio thread reads.
///
/// Before this layer existed, `MidiRoutingRes` was a resource with no write
/// path: the table's `set_routes` only marks it dirty, nothing in the crate ever
/// called `commit()`, and there was no way to say "channel 3 plays this synth"
/// from an app at all. Every assertion here reads through the arc the RT
/// pre-block holds, not through the resource, so a rule that stages without
/// publishing fails them.
/// (Was `tests/midi_route.rs`.)
mod midi_route {
    use std::sync::Arc;

    use bevy_app::prelude::*;
    use bevy_ecs::entity::Entity;

    use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::midi::{
        MidiRouteFallback, MidiRouteRule, MidiRoutingRes, MidiTargetRegistry, TuttiMidiPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::{AudioNode, RtPublish};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};
    use tutti_midi_types::{MidiRoutingSnapshot, MidiUnitId};
    use tutti_polysynth::{PolySynth, SynthConfig};

    const SAMPLE_RATE: f64 = 48_000.0;

    /// The snapshot half of the shared routing cell — what the RT pre-block reads.
    type RtView = Arc<RtPublish<MidiRoutingSnapshot>>;

    /// An app wired the way `build_into` leaves one, minus the audio device, with
    /// the routing table's RT half handed back so assertions can read it.
    fn app() -> (App, RtView) {
        let mut app = App::new();
        let mut net = Net::new(0, 2);
        let _backend = net.backend();
        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(TransportRes(tutti_core::transport::Transport::new(
            SAMPLE_RATE,
        )));
        app.insert_resource(AudioConfig {
            sample_rate: tutti_core::SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
            SAMPLE_RATE,
        ));

        let (routing, rt_view) = bevy_tutti::midi::test_support::routing_table_for_test();
        app.insert_resource(routing);

        // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
        // which panics without an `AssetServer` — a headless app supplies it.
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<PolySynth>();
        (app, rt_view)
    }

    fn spawn_synth(app: &mut App) -> (Entity, MidiUnitId) {
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let unit_id = synth.midi_port().unit_id();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.push(Box::new(synth))
        };
        let entity = app.world_mut().spawn(AudioNode(node)).id();
        (entity, unit_id)
    }

    /// Who does an event on `channel` reach, according to the RT snapshot?
    fn targets_on(rt_view: &RtView, channel: u8) -> Vec<MidiUnitId> {
        let snapshot = rt_view.read();
        snapshot
            .route(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::new(channel),
                60,
                0x8000,
            ))
            .collect()
    }

    /// The headline claim: a rule spawned in the ECS routes a channel to a synth.
    #[test]
    fn a_declared_route_reaches_the_rt_snapshot() {
        let (mut app, rt_view) = app();
        let (synth, unit_id) = spawn_synth(&mut app);

        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(3)).to(synth));
        app.update();

        assert!(
            targets_on(&rt_view, 3).contains(&unit_id),
            "a route declared in the ECS must reach the audio thread"
        );
        assert!(
            targets_on(&rt_view, 4).is_empty(),
            "and only on the channel it names"
        );
    }

    /// One rule, several destinations — a channel can layer two synths.
    #[test]
    fn a_rule_can_feed_several_synths() {
        let (mut app, rt_view) = app();
        let (lead, lead_id) = spawn_synth(&mut app);
        let (pad, pad_id) = spawn_synth(&mut app);

        app.world_mut().spawn(
            MidiRouteRule::for_channel(MidiChannel::new(0))
                .to(lead)
                .to(pad),
        );
        app.update();

        let targets = targets_on(&rt_view, 0);
        assert!(targets.contains(&lead_id) && targets.contains(&pad_id));
    }

    /// Removing a rule stops the routing — the rebuild is a whole-table replace, so
    /// a stale rule cannot survive in the published snapshot.
    ///
    /// This is what an additive rebuild would break: without the replace, the
    /// removed rule would go on routing forever.
    #[test]
    fn a_removed_rule_stops_routing() {
        let (mut app, rt_view) = app();
        let (synth, unit_id) = spawn_synth(&mut app);

        let rule = app
            .world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(3)).to(synth))
            .id();
        app.update();
        assert!(targets_on(&rt_view, 3).contains(&unit_id));

        app.world_mut().despawn(rule);
        app.update();

        assert!(
            !targets_on(&rt_view, 3).contains(&unit_id),
            "a despawned rule must stop routing"
        );
    }

    /// Disabling a rule keeps the declaration but stops the delivery.
    #[test]
    fn a_disabled_rule_routes_nothing() {
        let (mut app, rt_view) = app();
        let (synth, unit_id) = spawn_synth(&mut app);

        let rule = app
            .world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(3)).to(synth))
            .id();
        app.update();
        assert!(targets_on(&rt_view, 3).contains(&unit_id));

        app.world_mut()
            .entity_mut(rule)
            .get_mut::<MidiRouteRule>()
            .expect("the rule")
            .enabled = false;
        app.update();

        assert!(
            !targets_on(&rt_view, 3).contains(&unit_id),
            "a disabled rule stays declared but delivers nothing"
        );
    }

    /// An unmatched event reaches the fallback, and only when one is declared.
    #[test]
    fn the_fallback_catches_what_no_rule_matches() {
        let (mut app, rt_view) = app();
        let (synth, unit_id) = spawn_synth(&mut app);

        // No rules at all, so every channel is unmatched.
        app.update();
        assert!(
            targets_on(&rt_view, 7).is_empty(),
            "with no fallback declared, unmatched events go nowhere"
        );

        app.world_mut()
            .insert_resource(MidiRouteFallback(Some(synth)));
        app.update();

        assert!(
            targets_on(&rt_view, 7).contains(&unit_id),
            "once declared, the fallback catches unmatched channels"
        );
    }

    /// A rule naming an entity with no resolvable node is skipped, not panicked on,
    /// and picked up once the node arrives.
    ///
    /// An entity's node routinely materialises a frame after the entity does, so
    /// this is the ordinary case rather than an error path.
    #[test]
    fn an_unresolvable_target_is_skipped_then_picked_up() {
        let (mut app, rt_view) = app();

        // An entity with no `AudioNode` at all.
        let pending = app.world_mut().spawn_empty().id();
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(1)).to(pending));
        app.update();
        assert!(
            targets_on(&rt_view, 1).is_empty(),
            "nothing to resolve yet, and no panic"
        );

        // The node turns up.
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let unit_id = synth.midi_port().unit_id();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.push(Box::new(synth))
        };
        app.world_mut().entity_mut(pending).insert(AudioNode(node));
        app.update();

        assert!(
            targets_on(&rt_view, 1).contains(&unit_id),
            "the rule resolves once its target has a node — nothing about the rule \
             changed, so a rebuild gated on `Changed<MidiRouteRule>` alone would \
             leave it unresolved forever"
        );
    }

    /// The resource is the one the RT reads — a rebuild publishes into the shared
    /// cell, not a copy.
    #[test]
    fn the_rebuild_publishes_into_the_shared_cell() {
        let (mut app, rt_view) = app();
        let (synth, _) = spawn_synth(&mut app);

        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(2)).to(synth));
        app.update();

        assert_eq!(
            app.world().resource::<MidiRoutingRes>().route_count(),
            1,
            "the resource holds the compiled rule"
        );
        assert!(
            rt_view.read().has_routes(),
            "and the RT snapshot sees it — same cell, not two"
        );
    }
}

/// A MIDI node's sender reaches the bus, and leaves it again.
///
/// The half that was missing is the leaving: the previous layer registered a
/// sender inline in the soundfont spawner and never removed one anywhere, so the
/// bus grew by an entry per spawn and `MidiUnitId`s — from a monotonic counter,
/// never reused — accumulated for the life of the process.
///
/// The synth here is a `PolySynth` rather than a `SoundFontUnit` because the
/// latter needs a `.sf2` on disk; both own a `MidiInPort` and register the same
/// way, which is the whole point of resolving through a registry.
/// (Was `tests/midi_registration.rs`.)
mod midi_registration {
    use bevy_app::prelude::*;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin};
    use bevy_tutti::midi::{MidiRegistered, MidiTargetRegistry, TuttiMidiPlugin};
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::AudioNode;
    use tutti_polysynth::{PolySynth, SynthConfig};

    /// An app with a graph, a MIDI bus, and `PolySynth` registered as addressable.
    ///
    /// `MidiBusRes` cannot be `init_resource`d — it must be the instance the RT
    /// pre-block shares — so this uses the crate's own test constructor to stand one
    /// up without booting an audio device.
    fn app() -> App {
        let mut app = bare_app();
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<PolySynth>();
        app
    }

    /// The same app without any node type registered.
    ///
    /// The graph takes a `backend()`: removing a node marks it dirty, and the
    /// Commit-phase `commit_graph` asserts a backend exists before publishing. A
    /// backend-less graph is fine only for tests that never edit the topology.
    fn bare_app() -> App {
        let mut app = App::new();
        let mut net = Net::new(0, 2);
        let _backend = net.backend();
        app.insert_resource(AudioGraphRes(net));
        // `AudioEngineState::Running` is a claim about the whole engine block, and
        // systems gated on `engine_ready` take everything that block inserts as
        // plain `Res` — so a test asserting readiness has to supply them all.
        app.insert_resource(bevy_tutti::graph::TransportRes(
            tutti_core::transport::Transport::new(48_000.0),
        ));
        app.insert_resource(bevy_tutti::graph::AudioConfig {
            sample_rate: tutti_core::SampleRate(48_000.0),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
            48_000.0,
        ));
        // `engine_ready` claims every resource the engine block inserts is
        // present, and the route rebuild takes `MidiRoutingRes` as a plain
        // `ResMut` on that promise. A test asserting readiness supplies it.
        app.insert_resource(bevy_tutti::midi::test_support::routing_table_for_test().0);
        // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
        // which panics without an `AssetServer` — a headless app supplies it.
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
        app
    }

    /// Add a synth to the graph and give an entity its `AudioNode`.
    fn spawn_synth(app: &mut App) -> (bevy_ecs::entity::Entity, tutti_midi_types::MidiUnitId) {
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let unit_id = synth.midi_port().unit_id();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.push(Box::new(synth))
        };
        let entity = app.world_mut().spawn(AudioNode(node)).id();
        (entity, unit_id)
    }

    fn bus_has(app: &App, unit_id: tutti_midi_types::MidiUnitId) -> bool {
        app.world()
            .resource::<bevy_tutti::midi::MidiBusRes>()
            .contains(unit_id)
    }

    /// The bus learns a node's sender without anyone registering it by hand.
    #[test]
    fn a_midi_node_reaches_the_bus() {
        let mut app = app();
        let (entity, unit_id) = spawn_synth(&mut app);
        assert!(
            !bus_has(&app, unit_id),
            "not registered before a frame runs"
        );

        app.update();

        assert!(
            bus_has(&app, unit_id),
            "the synth's sender should be routable"
        );
        assert!(
            app.world().get::<MidiRegistered>(entity).is_some(),
            "and the entity should be marked as registered"
        );
    }

    /// The half that never existed: despawning takes the sender back off.
    #[test]
    fn a_despawned_node_leaves_the_bus() {
        let mut app = app();
        let (entity, unit_id) = spawn_synth(&mut app);
        app.update();
        assert!(bus_has(&app, unit_id));

        app.world_mut().despawn(entity);
        app.update();

        assert!(
            !bus_has(&app, unit_id),
            "a despawned synth must not keep routing MIDI — this is the leak"
        );
    }

    /// Removing just the node, keeping the entity, also unregisters — and the
    /// entity can register again if a node comes back.
    #[test]
    fn removing_the_node_unregisters_and_re_registering_works() {
        let mut app = app();
        let (entity, first_id) = spawn_synth(&mut app);
        app.update();
        assert!(bus_has(&app, first_id));

        app.world_mut().entity_mut(entity).remove::<AudioNode>();
        app.update();
        assert!(!bus_has(&app, first_id), "removing the node unregisters it");
        assert!(
            app.world().get::<MidiRegistered>(entity).is_none(),
            "the marker is cleared, so the entity is registrable again"
        );

        // A fresh unit on the same entity registers under its own new id.
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let second_id = synth.midi_port().unit_id();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.push(Box::new(synth))
        };
        app.world_mut().entity_mut(entity).insert(AudioNode(node));
        app.update();

        assert_ne!(first_id, second_id, "a new port mints a new id");
        assert!(bus_has(&app, second_id), "the replacement registers");
    }

    /// An entity whose node type was never registered is skipped, not panicked on,
    /// and does not block the ones that were.
    #[test]
    fn an_unregistered_node_type_is_skipped() {
        // Deliberately no `.register::<PolySynth>()`.
        let mut app = bare_app();

        let (entity, unit_id) = spawn_synth(&mut app);
        app.update();

        assert!(!bus_has(&app, unit_id), "nothing can resolve it");
        assert!(
            app.world().get::<MidiRegistered>(entity).is_none(),
            "and it stays unmarked, so it retries if the type is registered later"
        );
    }
}

/// Unwiring an entity from the graph takes `AudioNode` off, not a second handle.
///
/// `plugin_crash_detect_system` used to strip a crashed plugin of a second
/// `AudioEmitter` handle and remove its node from the graph by hand. That left
/// `AudioNode` in place, so the `On<Remove, AudioNode>` observers never fired:
/// the entity still claimed a node that was gone, and MIDI unregistration —
/// which keys on that same removal — never ran, leaking a sender on the bus for
/// the life of the process. `AudioEmitter` is gone now; `AudioNode` is the one
/// handle, and these pin the observers that hang off it.
///
/// # Why a synth rather than a crashed plugin
///
/// `PluginEmitter` carries a `PluginHandle`, which is eight `Arc<dyn …>`
/// collaborators around a live plugin subprocess — not constructible headless,
/// and a crash is not producible on demand. But the defect was never about
/// plugins: it was that removing the wrong component unwires nothing. That is
/// what these assert, on a `PolySynth` that registers through the same path.
/// (Was `tests/plugin_crash_unwire.rs`.)
mod plugin_crash_unwire {
    use bevy_app::prelude::*;
    use bevy_ecs::entity::Entity;

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin};
    use bevy_tutti::midi::{MidiBusRes, MidiTargetRegistry, TuttiMidiPlugin};
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::Net;
    use tutti_core::{AudioNode, NodeId};
    use tutti_midi_types::MidiUnitId;
    use tutti_polysynth::{PolySynth, SynthConfig};

    /// An app whose graph, bus, and registry are wired the way `build_into` leaves
    /// them, minus the audio device.
    fn app() -> App {
        let mut app = App::new();
        let mut net = Net::new(0, 2);
        // Removing a node marks the graph dirty, and Commit-phase `commit_graph`
        // asserts a backend exists before publishing.
        let _backend = net.backend();
        app.insert_resource(AudioGraphRes(net));
        app.insert_resource(bevy_tutti::graph::TransportRes(
            tutti_core::transport::Transport::new(48_000.0),
        ));
        app.insert_resource(bevy_tutti::graph::AudioConfig {
            sample_rate: tutti_core::SampleRate(48_000.0),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
            48_000.0,
        ));
        // `engine_ready` claims every resource the engine block inserts is
        // present, and the route rebuild takes `MidiRoutingRes` as a plain
        // `ResMut` on that promise. A test asserting readiness supplies it.
        app.insert_resource(bevy_tutti::midi::test_support::routing_table_for_test().0);
        // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
        // which panics without an `AssetServer` — a headless app supplies it.
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<PolySynth>();
        app
    }

    /// A synth bound to the graph the way every spawner binds one.
    fn spawn_synth(app: &mut App) -> (Entity, NodeId, MidiUnitId) {
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let unit_id = synth.midi_port().unit_id();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.push(Box::new(synth))
        };
        let entity = app.world_mut().spawn(AudioNode(node)).id();
        (entity, node, unit_id)
    }

    fn bus_has(app: &App, unit_id: MidiUnitId) -> bool {
        app.world().resource::<MidiBusRes>().contains(unit_id)
    }

    fn graph_has(app: &App, node: NodeId) -> bool {
        app.world().resource::<AudioGraphRes>().0.contains(node)
    }

    /// Removing `AudioNode` unwires both the graph node and the MIDI sender, with
    /// no help from the caller.
    ///
    /// This is what the crash path now does, and why it no longer touches the graph
    /// itself: one component removal drives both observers.
    #[test]
    fn removing_the_node_handle_unwires_the_graph_and_the_bus() {
        let mut app = app();
        let (entity, node, unit_id) = spawn_synth(&mut app);
        app.update();
        assert!(bus_has(&app, unit_id), "registered to begin with");
        assert!(graph_has(&app, node), "and in the graph");

        app.world_mut().entity_mut(entity).remove::<AudioNode>();
        app.update();

        assert!(
            !graph_has(&app, node),
            "the observer removes the node — the crash system must not do it by hand"
        );
        assert!(
            !bus_has(&app, unit_id),
            "and MIDI unregistration rides on the same removal"
        );
    }

    /// Despawning the whole entity unwires it too — the crash path's neighbour.
    ///
    /// `On<Remove, AudioNode>` fires for a despawn as well as an explicit removal,
    /// so a plugin that is despawned rather than stripped leaks nothing either.
    #[test]
    fn despawning_the_entity_unwires_it_as_well() {
        let mut app = app();
        let (entity, node, unit_id) = spawn_synth(&mut app);
        app.update();

        app.world_mut().despawn(entity);
        app.update();

        assert!(!graph_has(&app, node), "the node goes with the entity");
        assert!(!bus_has(&app, unit_id), "and so does the sender");
    }
}

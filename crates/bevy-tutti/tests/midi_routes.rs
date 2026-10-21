//! MIDI wiring declared in the ECS reaching the graph: route rules wire the
//! hardware input node's ports to their targets, a keyboard's `LiveMidiInput`
//! inserts a queue node wired to its entity, and removing a node takes
//! everything wired to it with it.
//!
//! - `routes` — a rule becomes event edges from the input node's ports.
//! - `keyboard` — a `LiveMidiInput` reaches its entity, and goes with it.
//! - `unwire` — removing `AudioNode` (a crashed plugin's teardown, a despawn)
//!   removes the node from the graph.

#![cfg(all(feature = "midi", feature = "synth"))]

#[macro_use]
mod common;

use std::collections::BTreeSet;

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, EventSource, GraphReconcilePlugin, SpawnAudioNode, TransportRes,
};
use bevy_tutti::midi::{MidiEngineNodes, MidiRouteFallback, MidiRouteRule, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::{AudioNode, SampleRate};
use tutti_midi_runtime::{MidiInputNode, CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_midi_types::MidiChannel;
use tutti_polysynth::{PolySynth, SynthConfig};

const SAMPLE_RATE: f64 = 48_000.0;

/// An app with the engine's MIDI input node (no wire) as `build_into`
/// leaves it, minus the audio device.
fn app() -> App {
    let mut app = App::new();
    let mut graph = AudioGraphRes::headless(0, 2);
    let (input_node, _) = graph.insert(MidiInputNode::new(None));
    app.insert_resource(graph);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        channels: tutti_core::ChannelLayout::STEREO,
    });
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test());
    // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
    // which panics without an `AssetServer` — a headless app supplies it.
    app.add_plugins((
        bevy_app::TaskPoolPlugin::default(),
        bevy_asset::AssetPlugin::default(),
    ));
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    let input = app.world_mut().spawn(input_node).id();
    app.insert_resource(MidiEngineNodes {
        input,
        input_node,
        clock: input,
        hardware_out: input,
    });
    app
}

/// A synth inserted as a graph node.
fn synth(app: &mut App) -> Entity {
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let entity = app.world_mut().commands().spawn_audio_node(synth).id();
    app.world_mut().flush();
    entity
}

fn node_of(app: &App, entity: Entity) -> AudioNode {
    *app.world()
        .get::<AudioNode>(entity)
        .expect("bound to a node")
}

mod routes {
    use super::*;

    /// The input node's ports feeding `entity`'s event input.
    fn ports(app: &App, entity: Entity) -> BTreeSet<u16> {
        let input = app.world().resource::<MidiEngineNodes>().input_node;
        app.world()
            .resource::<AudioGraphRes>()
            .event_sources(node_of(app, entity), 0)
            .into_iter()
            .filter(|s| s.node == input)
            .map(|s: EventSource| s.port)
            .collect()
    }

    fn channelless() -> u16 {
        u16::try_from(CHANNELLESS_PORT).unwrap()
    }

    /// **A channel rule wires that channel's port and the channelless one;
    /// an any-channel rule all seventeen; two rules on one target the
    /// union.**
    ///
    /// Mutation (run): the rebuild giving a channel rule its channel's port
    /// alone (`input_ports` without the channelless port) → fails.
    #[test]
    fn a_rule_wires_its_channels_ports() {
        let mut app = app();
        let (lead, pad) = (synth(&mut app), synth(&mut app));
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(2)).to(lead));
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::new(5)).to(lead));
        app.world_mut().spawn(MidiRouteRule::any_channel().to(pad));
        app.update();
        assert_eq!(ports(&app, lead), BTreeSet::from([2, 5, channelless()]));
        assert_eq!(
            ports(&app, pad),
            (0..u16::try_from(MIDI_INPUT_PORTS).unwrap()).collect()
        );
    }

    /// **The fallback takes every port no rule covers**: with a rule on
    /// channel 0, the channels 1–15 (and not the channelless port, which the
    /// rule's target takes).
    ///
    /// Mutation (run): the fallback taking every port → fails.
    #[test]
    fn the_fallback_takes_the_uncovered_ports() {
        let mut app = app();
        let (lead, sampler) = (synth(&mut app), synth(&mut app));
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(lead));
        app.insert_resource(MidiRouteFallback(Some(sampler)));
        app.update();
        assert_eq!(ports(&app, sampler), (1..16).collect());
    }

    /// **A disabled rule routes nothing, and removing a rule unwires its
    /// target.**
    ///
    /// Mutation (run, against `tutti_session::Midi::set_routes`): not removing
    /// the feeds of a node no rule reaches any more
    /// (the session's `routed` set unused) → the ports stay → fails.
    #[test]
    fn removing_or_disabling_a_rule_unwires_its_target() {
        let mut app = app();
        let lead = synth(&mut app);
        let rule = app
            .world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(lead))
            .id();
        app.update();
        assert!(!ports(&app, lead).is_empty());

        app.world_mut().entity_mut(rule).insert(
            MidiRouteRule::for_channel(MidiChannel::FIRST)
                .to(lead)
                .disabled(),
        );
        app.update();
        assert!(ports(&app, lead).is_empty(), "disabled");

        app.world_mut()
            .entity_mut(rule)
            .insert(MidiRouteRule::for_channel(MidiChannel::FIRST).to(lead));
        app.update();
        assert!(!ports(&app, lead).is_empty(), "re-armed");
        app.world_mut().despawn(rule);
        app.update();
        assert!(ports(&app, lead).is_empty(), "removed");
    }

    /// **A rule naming a target that has no node yet wires it once it has
    /// one**: the rule resolves entities, and the event wiring re-derives
    /// every frame.
    #[test]
    fn a_rule_reaches_a_target_spawned_after_it() {
        let mut app = app();
        let lead = app.world_mut().spawn_empty().id();
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(lead));
        app.update();
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.insert(synth).0
        };
        app.world_mut().entity_mut(lead).insert(node);
        app.update();
        assert_eq!(ports(&app, lead), BTreeSet::from([0, channelless()]));
    }
}

mod keyboard {
    use super::*;
    use bevy_tutti::midi::{LiveMidi, LiveMidiInput};

    /// **A `LiveMidiInput` inserts a queue node wired to its entity, and
    /// removing it (or despawning the entity) removes the node.**
    ///
    /// Mutation (run): `detach_live_midi` not removing the node → it stays in
    /// the graph → fails.
    #[test]
    fn a_keyboard_reaches_its_entity_and_goes_with_it() {
        let mut app = app();
        let lead = synth(&mut app);
        app.world_mut().entity_mut(lead).insert(LiveMidiInput);
        app.update();
        let queue = app
            .world()
            .get::<LiveMidi>(lead)
            .expect("the keyboard is attached")
            .node();
        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .event_sources(node_of(&app, lead), 0),
            vec![queue.into()]
        );

        app.world_mut().entity_mut(lead).remove::<LiveMidiInput>();
        app.update();
        assert!(app.world().get::<LiveMidi>(lead).is_none());
        assert!(!app.world().resource::<AudioGraphRes>().contains(queue));

        app.world_mut().entity_mut(lead).insert(LiveMidiInput);
        app.update();
        let queue = app.world().get::<LiveMidi>(lead).unwrap().node();
        app.world_mut().despawn(lead);
        app.update();
        assert!(!app.world().resource::<AudioGraphRes>().contains(queue));
    }

    /// **A keyboard follows its entity onto a new node bound after the old
    /// one was removed.** Removing `AudioNode` removes the old node, and its
    /// feeds with it (`Graph::remove`), so nothing is left to move; the live
    /// input re-asserts its feed on the entity's node whenever that changes.
    ///
    /// Mutations (run), each fails: `attach_live_midi` not
    /// re-aiming the queue on a changed `AudioNode` (`Midi::retarget`
    /// skipped); the session's `retarget` not feeding the new target → the
    /// new synth's event input is empty.
    #[test]
    fn a_keyboard_follows_its_entity_onto_a_node_bound_after_a_removal() {
        use bevy_tutti::graph::InsertAudioNode;

        let mut app = app();
        let lead = synth(&mut app);
        app.world_mut().entity_mut(lead).insert(LiveMidiInput);
        app.update();
        let queue = app.world().get::<LiveMidi>(lead).unwrap().node();
        let old = node_of(&app, lead);

        app.world_mut().entity_mut(lead).remove::<AudioNode>();
        app.world_mut().flush();
        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        app.world_mut()
            .commands()
            .entity(lead)
            .insert_audio_node(synth);
        app.world_mut().flush();
        app.update();

        let new = node_of(&app, lead);
        assert_ne!(new, old, "the entity is bound to a new node");
        assert_eq!(
            app.world().get::<LiveMidi>(lead).unwrap().node(),
            queue,
            "the same keyboard"
        );
        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .event_sources(new, 0),
            vec![queue.into()],
            "the keyboard feeds the new node"
        );
    }

    /// **A `LiveMidiInput` added before the graph exists is attached once it
    /// arrives.** The entity and its node are there, the `AudioGraphRes` is
    /// not (taken out) for a few frames; on the frame it is back, the entity
    /// gets its `LiveMidi`, fed into its node.
    ///
    /// Mutation (run): `attach_live_midi` gated by an early return on a
    /// missing graph rather than the plugin's `run_if` (the system runs,
    /// consuming the `Added` tick) → no
    /// `LiveMidi` → fails.
    #[test]
    fn a_keyboard_declared_before_the_graph_attaches_when_it_arrives() {
        let mut app = app();
        let lead = synth(&mut app);
        app.update();
        let graph = app
            .world_mut()
            .remove_resource::<AudioGraphRes>()
            .expect("the graph");
        app.world_mut().entity_mut(lead).insert(LiveMidiInput);
        app.update();
        app.update();
        assert!(app.world().get::<LiveMidi>(lead).is_none(), "no graph yet");

        app.insert_resource(graph);
        app.update();
        let queue = app
            .world()
            .get::<LiveMidi>(lead)
            .expect("attached once the graph is back")
            .node();
        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .event_sources(node_of(&app, lead), 0),
            vec![queue.into()]
        );
    }

    /// **A keyboard on a node that takes no MIDI gets none, and gets one
    /// when its entity is re-bound to a synth.** The first attach is
    /// refused (`Error::NoEventInput`, warned); the rebind is a changed
    /// `AudioNode`, which tries again.
    ///
    /// Mutation (run): the attach query filtering on `Added<LiveMidiInput>`
    /// alone (not `Changed<AudioNode>`) → the rebind is not seen → no
    /// `LiveMidi` → fails.
    #[test]
    fn a_keyboard_on_a_node_without_midi_attaches_after_a_rebind_to_a_synth() {
        use bevy_tutti::graph::InsertAudioNode;
        use tutti_graph::ForkByClone;
        use tutti_nodes::testing::Const;

        let mut app = app();
        let lead = app
            .world_mut()
            .commands()
            .spawn_audio_node(ForkByClone(Const::mono(0.0)))
            .id();
        app.world_mut().flush();
        app.world_mut().entity_mut(lead).insert(LiveMidiInput);
        app.update();
        app.update();
        assert!(
            app.world().get::<LiveMidi>(lead).is_none(),
            "a node with no event input takes no keyboard"
        );

        let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
        app.world_mut()
            .commands()
            .entity(lead)
            .insert_audio_node(synth);
        app.world_mut().flush();
        app.update();
        let queue = app
            .world()
            .get::<LiveMidi>(lead)
            .expect("attached after the rebind")
            .node();
        assert_eq!(
            app.world()
                .resource::<AudioGraphRes>()
                .event_sources(node_of(&app, lead), 0),
            vec![queue.into()]
        );
    }

    /// **What a keyboard sends arrives at its synth**: a note through
    /// `LiveMidi` sounds.
    #[test]
    fn a_keyboard_note_sounds() {
        let mut app = app();
        let lead = synth(&mut app);
        let node = node_of(&app, lead);
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_outputs_from(node);
        app.world_mut().entity_mut(lead).insert(LiveMidiInput);
        app.update();
        let keys = app.world().get::<LiveMidi>(lead).unwrap().clone();
        assert!(keys.note_on(MidiChannel::FIRST, 69, 100));
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let mut peak = 0.0f32;
        for _ in 0..2_048 {
            let mut frame = [0.0f32; 2];
            graph.render_frame(&mut frame);
            peak = peak.max(frame[0].abs());
        }
        assert!(peak > 0.01, "the keyboard's note sounds (peak {peak})");
    }
}

/// Unwiring an entity from the graph takes `AudioNode` off, not a second
/// handle: the `On<Remove, AudioNode>` observer removes its node, on a
/// removal or a despawn alike. (A crashed plugin's teardown takes this path;
/// a synth stands in for it, since a crash is not producible on demand.)
mod unwire {
    use super::*;

    fn graph_has(app: &App, node: AudioNode) -> bool {
        app.world().resource::<AudioGraphRes>().contains(node)
    }

    #[test]
    fn removing_the_node_handle_removes_the_node() {
        let mut app = app();
        let entity = synth(&mut app);
        app.update();
        let node = node_of(&app, entity);
        assert!(graph_has(&app, node));
        app.world_mut().entity_mut(entity).remove::<AudioNode>();
        app.update();
        assert!(!graph_has(&app, node));
    }

    #[test]
    fn despawning_the_entity_removes_the_node() {
        let mut app = app();
        let entity = synth(&mut app);
        app.update();
        let node = node_of(&app, entity);
        app.world_mut().despawn(entity);
        app.update();
        assert!(!graph_has(&app, node));
    }
}

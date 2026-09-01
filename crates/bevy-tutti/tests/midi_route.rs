//! A route declared in the ECS reaches the snapshot the audio thread reads.
//!
//! Before this layer existed, `MidiRoutingRes` was a resource with no write
//! path: the table's `set_routes` only marks it dirty, nothing in the crate ever
//! called `commit()`, and there was no way to say "channel 3 plays this synth"
//! from an app at all. Every assertion here reads through the arc the RT
//! pre-block holds, not through the resource, so a rule that stages without
//! publishing fails them.

#![cfg(all(feature = "midi", feature = "synth"))]

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
        channels: Default::default(),
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

    app.world_mut()
        .spawn(MidiRouteRule::for_channel(MidiChannel::new(0)).to(lead).to(pad));
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

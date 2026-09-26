//! A hosted plugin, loaded through the real request pipeline, is driven through
//! the shadow captured at load — never through the graph.
//!
//! `plugin_host::bind` and `plugin_host::latency` reach a plugin through its
//! [`PluginShadow`]: the node's `PluginControls`, taken from the loaded client
//! in `plugin_load_promote` (`CapturedControls::for_plugin`) before it is bound
//! and moves into the graph. This file puts a real plugin behind that path —
//! the reference CLAP plugin, loaded by a real `plugin-server` subprocess — and
//! checks each consumer of the shadow did its job:
//!
//! - the shadow is there, bound to the entity's node;
//! - the meter binding ran (`PluginMeterBound`), which it only does through
//!   the shadow;
//! - the graph plans PDC against the node's declared latency, and a latency
//!   the plugin changes reaches it through the poll, which reads only the
//!   shadow;
//! - the MIDI target was captured too, from the plugin's own port.
//!
//! # Mutation
//!
//! Replacing `CapturedControls::for_plugin(&client)` in `plugin_load_promote`
//! with `CapturedControls::default()` fails with every observation empty: no
//! MIDI target, no shadow, so no meter bind, and the latency change never
//! reaches the graph. Dropping only the `plugin` field of `for_plugin`
//! (`plugin: None`) fails with the MIDI target still present and the others
//! empty, which is how the test tells the two captures apart.
//!
//! # The two artifacts
//!
//! The plugin is a dev-dependency, so cargo builds its cdylib alongside this
//! test. The server is not (it would be a dependency cycle through
//! `tutti-plugin`), so it must be built first: `cargo build -p
//! tutti-plugin-server`. Both are looked up beside this test binary.

#![cfg(feature = "plugin")]

#[macro_use]
mod common;

use common::plugin::{clap_probe, plugin_server};

use std::time::{Duration, Instant};

use bevy_app::prelude::*;
use bevy_ecs::schedule::IntoScheduleConfigs;

use bevy_tutti::graph::{
    AudioGraphRes, GraphDirty, GraphReconcilePlugin, GraphReconcileSystems, MasterSources,
    MetronomeRes, TransportRes,
};
use bevy_tutti::midi::MidiTarget;
use bevy_tutti::plugin_host::{
    PluginLoadTerminated, PluginMeterBound, PluginRequest, PluginShadow, TuttiHostingPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::{ClickState, Transport};
use tutti_core::{AudioNode, SampleRate, Samples};
use tutti_plugin::catalog::PluginId;

const SAMPLE_RATE: f64 = 48_000.0;
/// The plugin's pipeline chunk here: one device callback when the graph knows
/// the device's (doc 013, decision 8 reversed); this headless graph knows none,
/// so the chunk is its `MaxBlock` (bevy-tutti's `NATIVE_MAX_BLOCK`).
const CHUNK: usize = 1024;

fn app() -> App {
    // SAFETY: nextest runs this test in its own process, and nothing else in it
    // reads the environment concurrently; the server path is the only lever
    // `Plugin::open_with` offers.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server()) };

    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    // The meter is what `plugin_bind_meter` installs; it waits without it.
    // (The transport is the graph's `Env`, which needs nothing installed; the
    // modulation half binds params against `TransportRes`.)
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(MetronomeRes(std::sync::Arc::new(ClickState::new())));
    app.add_plugins((GraphReconcilePlugin, TuttiHostingPlugin));
    app
}

#[test]
fn a_loaded_plugin_is_bound_and_polled_through_its_shadow() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn(PluginRequest {
            id: PluginId::from_path(clap_probe()),
            sample_rate: SampleRate(SAMPLE_RATE),
            ..Default::default()
        })
        .id();
    // Declared as a host would: the plugin on the master. Needed as well as
    // natural — with a latent node in the graph, `wire::rebuild`'s self-check
    // compares latency plans per output channel, and an undeclared master gives
    // the value no channels to compare against the engine's two.
    app.insert_resource(MasterSources::from(entity));

    // The load runs on the task pool; frames keep polling until it lands.
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.world().get::<PluginLoadTerminated>(entity).is_none() {
        assert!(
            Instant::now() < deadline,
            "the plugin never finished loading"
        );
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    // One more frame for the systems ordered after `Spawn` to see the node.
    app.update();

    let node = app
        .world()
        .get::<AudioNode>(entity)
        .expect("a loaded plugin is bound to a graph node")
        .0;
    // Observed together, so a failure shows which consumers of the capture
    // went quiet rather than stopping at the first.
    let world = app.world();
    let observed = Observed {
        midi_target_node: world.get::<MidiTarget>(entity).map(MidiTarget::node),
        shadow_node: world.get::<PluginShadow>(entity).map(PluginShadow::node),
        meter_bound: world.get::<PluginMeterBound>(entity).is_some(),
    };
    assert_eq!(
        observed,
        Observed {
            // `PluginClient` is registered for MIDI by the hosting plugin, so its
            // port is captured at load, for this node.
            midi_target_node: Some(node),
            // The shadow is captured at load, for this node.
            shadow_node: Some(node),
            // Installed through the shadow; no shadow, no binding.
            meter_bound: true,
        },
    );
    // And the graph plans PDC against the node's whole latency: the plugin's
    // figure plus the one chunk its out-of-process pipeline holds
    // ([`CHUNK`]), as the node's `Shape` declares it. Read off the shape the
    // editor holds. (Handing the editor the plugin's own figure instead —
    // `PluginControls::latency` — drops the pipeline chunk, and fails here.)
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .node_latency(AudioNode(node)),
        Samples(tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize + CHUNK),
        "the graph plans PDC against the node's latency"
    );

    // Idle frames after the load leave the graph clean: the poll compares
    // the plugin's declared latency with the editor's own figure, and while
    // the two agree it raises nothing, so nothing is recompiled or
    // republished frame after frame. Observed between the poll and the
    // commit (which clears the flag), by a probe system (`record_dirty`).
    //
    // Mutation (run): raise `GraphDirty` in the poll whether or not the
    // figure moved → the probe sees it set → fails.
    app.init_resource::<DirtySeen>();
    app.add_systems(
        Update,
        record_dirty
            .after(bevy_tutti::plugin_host::plugin_latency_poll)
            .before(GraphReconcileSystems::Commit),
    );
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<DirtySeen>().0,
        0,
        "an unchanged plugin latency marked the graph dirty"
    );

    // A latency change after load — what a plugin's `latency.changed`
    // delivers into its cell — reaches the graph through the poll: a `Shape`
    // change at the next commit. That is the one path: the editor read the
    // node's shape at insert and holds that figure until told.
    //
    // Mutation (run): the poll not calling `refresh_node_latency` → the graph
    // keeps planning against the load-time figure, and this fails.
    app.world()
        .get::<PluginShadow>(entity)
        .and_then(|s| s.controls_for(&AudioNode(node)))
        .expect("the shadow is for this node")
        .set_latency(Samples(300));
    app.update();
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .node_latency(AudioNode(node)),
        Samples(300 + CHUNK),
        "a changed plugin latency reaches the graph"
    );

    // A figure past what PDC compensates is clamped, not refused: the editor
    // refuses one past `MAX_NODE_LATENCY` outright, and the poll would not
    // ask again until the plugin's figure moved.
    //
    // Mutation (run): dropping the clamp in `clamp_latency` → the editor
    // refuses the figure, the graph keeps 364 and this fails.
    app.world()
        .get::<PluginShadow>(entity)
        .and_then(|s| s.controls_for(&AudioNode(node)))
        .expect("the shadow is for this node")
        .set_latency(Samples(10_000_000));
    app.update();
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .node_latency(AudioNode(node)),
        tutti_core::latency::MAX_NODE_LATENCY,
        "clamped to what PDC compensates"
    );
}

/// Frames on which `GraphDirty` was set between the latency poll and the
/// commit.
#[derive(bevy_ecs::prelude::Resource, Default)]
struct DirtySeen(usize);

fn record_dirty(
    dirty: bevy_ecs::prelude::Res<GraphDirty>,
    mut seen: bevy_ecs::prelude::ResMut<DirtySeen>,
) {
    if dirty.0 {
        seen.0 += 1;
    }
}

/// What the capture's consumers left on the entity.
#[derive(Debug, PartialEq)]
struct Observed {
    midi_target_node: Option<tutti_core::dsp::NodeId>,
    shadow_node: Option<tutti_core::dsp::NodeId>,
    meter_bound: bool,
}

/// Spawn a request for the probe on the master and run frames until it loads.
fn load_probe_entity(app: &mut App) -> bevy_ecs::entity::Entity {
    let entity = app
        .world_mut()
        .spawn(PluginRequest {
            id: PluginId::from_path(clap_probe()),
            sample_rate: SampleRate(SAMPLE_RATE),
            ..Default::default()
        })
        .id();
    app.insert_resource(MasterSources::from(entity));
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.world().get::<PluginLoadTerminated>(entity).is_none() {
        assert!(
            Instant::now() < deadline,
            "the plugin never finished loading"
        );
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    app.update();
    entity
}

/// A crossfaded plugin is bound again: the incoming `PluginClient` gets the
/// meter (and, with modulation, its param automation) installed, and the
/// latency poll follows the incoming plugin's figure.
///
/// The binding systems latch on `PluginMeterBound` / `PluginParamsBound`,
/// which describe the *outgoing* client. A crossfade keeps the entity and its
/// markers, so unless the re-capture clears them the incoming client never has
/// anything installed — a plugin told 4/4 from bar 0 with no automation, and
/// nothing logs.
///
/// # Mutation
///
/// Deleting the marker-clearing block at the top of
/// `CapturedControls::replace` fails the meter (and params) assertion: the
/// incoming client's slots stay empty. Not replacing the shadow at all
/// (dropping the `plugin` arm of `replace`) fails the latency assertion too,
/// since the poll keeps reading the outgoing client's cell.
#[test]
fn a_crossfaded_plugin_is_bound_again() {
    let mut app = app();
    #[cfg(feature = "modulation")]
    app.add_plugins(bevy_tutti::modulation::TuttiModulationPlugin);
    let entity = load_probe_entity(&mut app);
    #[cfg(feature = "modulation")]
    app.world_mut().entity_mut(entity).insert(
        bevy_tutti::modulation::ModParamRange::default().with(
            tutti_types::ParamAddr::Id(0),
            0.5,
            0.0,
            1.0,
        ),
    );
    app.update();
    assert!(
        app.world().get::<PluginMeterBound>(entity).is_some(),
        "precondition: the outgoing plugin was bound"
    );

    // A second instance of the probe, told a different latency so the poll's
    // record says which client it read.
    let incoming = tutti_plugin::handles::PluginClient::new(
        tutti_plugin::BridgeConfig::default(),
        clap_probe(),
        SampleRate(SAMPLE_RATE),
    )
    .expect("load a second instance of the reference plugin");
    const INCOMING_LATENCY: usize = 211;
    incoming.set_latency(Samples(INCOMING_LATENCY));
    let controls = incoming.controls();
    bevy_tutti::graph::crossfade_plugin_node(&mut app.world_mut().commands(), entity, incoming);
    app.update();
    app.update();

    assert!(
        controls.has_meter(),
        "the incoming plugin must get the meter installed"
    );
    #[cfg(feature = "modulation")]
    assert!(
        controls.has_param_automation_source(),
        "and its param automation"
    );
    let node = *app.world().get::<AudioNode>(entity).expect("still bound");
    assert_eq!(
        app.world().resource::<AudioGraphRes>().node_latency(node),
        Samples(INCOMING_LATENCY + CHUNK),
        "the graph plans against the incoming plugin's declared latency"
    );
    // And a later change of the incoming plugin's latency is the one the
    // poll hands the graph: it reads the incoming plugin's shadow.
    controls.set_latency(Samples(INCOMING_LATENCY + 7));
    app.update();
    assert_eq!(
        app.world().resource::<AudioGraphRes>().node_latency(node),
        Samples(INCOMING_LATENCY + 7 + CHUNK),
        "the latency poll must read the incoming plugin"
    );

    // And when the node goes — the dead-plugin teardown takes `AudioNode` off
    // exactly like this — the shadow goes with it, so a dead plugin's slots
    // are not kept alive by the entity. Mutation: dropping the
    // `drop_captured` call from `reconcile_node_despawn` fails this.
    app.world_mut().entity_mut(entity).remove::<AudioNode>();
    app.update();
    assert!(app.world().get::<PluginShadow>(entity).is_none());
}

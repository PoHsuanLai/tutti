//! A crashed plugin comes back on the same entity, through the real request
//! pipeline and a real `plugin-server`: Bevy only translates what the
//! session decides (`plugin_host::health`), so the entity keeps its identity,
//! its `AudioNode` and its place in the master's declared sources, and is told
//! with entity events.
//!
//! The reference plugin aborts on its 3rd block in the first server only
//! (`TUTTI_CLAP_PROBE_ONCE_FILE`); the second server is the replacement.
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! without it this fails, naming that command.
//!
//! # Mutation
//!
//! `plugin_health_poll` not landing the reload (`reloads` never polled): no
//! `PluginRecovered`, the entity stays `Recovering` (run, failed). Not
//! re-inserting `PluginEmitter` on `PluginRecovered`: the emitter's server pid
//! is still the dead server's (run, failed).

#![cfg(feature = "plugin")]

#[macro_use]
mod common;

use common::plugin::{clap_probe, plugin_server};

use std::time::{Duration, Instant};

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::AudioEngineState;
use bevy_tutti::graph::AudioNode;
use bevy_tutti::graph::{
    AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
};
use bevy_tutti::plugin_host::{
    PluginEmitter, PluginHealth, PluginLiveness, PluginLoadTerminated, PluginRecovered,
    PluginReloading, PluginRequest, TuttiHostingPlugin,
};
use tutti_nodes::ClickState;
use tutti_plugin::catalog::PluginId;
use tutti_transport::Transport;
use tutti_types::SampleRate;

const SAMPLE_RATE: f64 = 48_000.0;

#[derive(Component)]
struct Mine;

/// What the entity events said, in order.
#[derive(Resource, Default)]
struct Told(Vec<&'static str>);

fn app(once: &str, crash_on_block: u32) -> App {
    // SAFETY: nextest runs this test in its own process, and nothing else in it
    // reads the environment concurrently.
    unsafe {
        std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server());
        std::env::set_var("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
        std::env::set_var("TUTTI_CLAP_PROBE_ONCE_FILE", once);
        std::env::set_var(
            "TUTTI_CLAP_PROBE_CRASH_ON_BLOCK",
            crash_on_block.to_string(),
        );
        std::env::set_var("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    }
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(MetronomeRes(std::sync::Arc::new(ClickState::new())));
    app.init_resource::<Told>();
    app.add_plugins((GraphReconcilePlugin, TuttiHostingPlugin));
    #[cfg(feature = "modulation")]
    app.add_plugins(bevy_tutti::modulation::TuttiModulationPlugin);
    app.add_observer(|_: On<PluginReloading>, mut told: ResMut<Told>| told.0.push("reloading"));
    app.add_observer(|_: On<PluginRecovered>, mut told: ResMut<Told>| told.0.push("recovered"));
    app
}

/// One app frame, and a plugin chunk's worth of sample frames rendered from
/// the graph (the pipeline sends the server one block per chunk). Whether
/// any was non-zero.
fn frame(app: &mut App) -> bool {
    app.update();
    let mut heard = false;
    for _ in 0..CHUNK {
        let mut out = [0.0f32; 2];
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .render_frame(&mut out);
        heard |= out.iter().any(|&x| x != 0.0);
    }
    std::thread::sleep(Duration::from_millis(5));
    heard
}

/// The plugin's pipeline chunk in this headless graph: its `MaxBlock`.
const CHUNK: usize = 1024;

#[test]
fn a_crashed_plugin_comes_back_on_the_same_entity() {
    let once = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("bevy-recovery.once");
    let _ = std::fs::remove_file(&once);
    let mut app = app(&once.to_string_lossy(), 3);
    let entity = app
        .world_mut()
        .spawn((
            Mine,
            PluginRequest {
                id: PluginId::from_path(clap_probe()),
                sample_rate: SampleRate(SAMPLE_RATE),
                ..Default::default()
            },
        ))
        .id();
    app.insert_resource(MasterSources::from(entity));

    let deadline = Instant::now() + Duration::from_secs(30);
    while app.world().get::<PluginLoadTerminated>(entity).is_none() {
        assert!(Instant::now() < deadline, "the plugin never loaded");
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    let node = *app.world().get::<AudioNode>(entity).expect("bound");
    let pid = |app: &App| {
        app.world()
            .get::<PluginEmitter>(entity)
            .expect("an emitter")
            .handle
            .server_pid()
    };
    let first_pid = pid(&app);
    assert!(first_pid.is_some());

    // Render through the crash and the recovery.
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.world().resource::<Told>().0 != ["reloading", "recovered"] {
        assert!(
            Instant::now() < deadline,
            "told {:?}; status {:?}; handle {:?}",
            app.world().resource::<Told>().0,
            app.world()
                .get::<PluginHealth>(entity)
                .map(|h| h.status.clone()),
            app.world()
                .get::<PluginEmitter>(entity)
                .map(|e| (e.handle.status(), e.handle.block_counts())),
        );
        frame(&mut app);
    }

    let world = app.world();
    assert!(world.get::<Mine>(entity).is_some(), "the same entity");
    assert_eq!(world.get::<AudioNode>(entity), Some(&node), "the same node");
    assert_ne!(pid(&app), first_pid, "the emitter holds the replacement");
    assert_eq!(
        world.get::<PluginHealth>(entity).map(|h| h.status.clone()),
        Some(PluginLiveness::Healthy)
    );
    // The master still reads the entity, and the replacement is heard.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !frame(&mut app) {
        assert!(Instant::now() < deadline, "the replacement renders nothing");
    }
}

/// **A modulation lane on the plugin survives the replacement.** The lane
/// (a macro of -0.25 on the gain parameter, id 77) is a node of its own
/// feeding the plugin's event input. The replacement takes the plugin's node,
/// so the declarations still resolve; the reconciler rebuilds the lane for the
/// new unit (the node's generation moved), as it does for a plugin swapped by
/// hand, and the lane's value reaches the replacement, which loaded at its
/// default (no snapshot was due): the gain reads default minus 0.25 again.
///
/// Mutation (run, failed): `install_reload` building the replacement without
/// the old plugin's mirror (`hosted.mirror = None`): the lane has no knob to
/// start from and never reaches the replacement.
#[cfg(feature = "modulation")]
#[test]
fn a_modulation_lane_on_the_plugin_survives_its_replacement() {
    use bevy_tutti::modulation::{Macro, ModRange, ModRoute, ModSource, ParamRef};
    use tutti_plugin::handles::{Normalized, ParamId};
    use tutti_types::Depth;

    let once = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("bevy-recovery-lane.once");
    let _ = std::fs::remove_file(&once);
    let mut app = app(&once.to_string_lossy(), 8);
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
    loop {
        let world = app.world();
        let loaded = world.get::<PluginLoadTerminated>(entity).is_some()
            && world.get::<AudioNode>(entity).is_some_and(|&node| {
                world
                    .resource::<AudioGraphRes>()
                    .with_plugin(node, |p| p.param_values().is_some_and(|m| m.is_ready()))
                    == Some(true)
            });
        if loaded {
            break;
        }
        assert!(Instant::now() < deadline, "the plugin never loaded");
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }

    let gain = ParamRef::plugin(entity, 77);
    let world = app.world_mut();
    world.spawn(ModRange::new(gain, 0.0, 1.0));
    let source = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(source, gain).with_depth(Depth(-0.25)));
    let lane_of = |app: &App| {
        app.world()
            .resource::<AudioGraphRes>()
            .modulation()
            .lane(gain)
            .map(|(node, _)| node)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while lane_of(&app).is_none() {
        assert!(Instant::now() < deadline, "no lane on the gain");
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    let lane = lane_of(&app);
    let value = |app: &App| {
        app.world()
            .get::<PluginEmitter>(entity)
            .expect("the handle")
            .handle
            .params()
            .parameter_value(ParamId::new(77))
    };
    // The default is 60/72 of the range; the lane moves it down a quarter.
    let lowered = Normalized::new(60.0 / 72.0 - 0.25);
    let near = |v: Option<Normalized>| v.is_some_and(|v| (v.get() - lowered.get()).abs() < 1e-3);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(value(&app)) {
        assert!(
            Instant::now() < deadline,
            "the lane never reached the plugin: {:?}",
            value(&app)
        );
        frame(&mut app);
    }

    // Through the crash (block 8) and the recovery.
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.world().resource::<Told>().0 != ["reloading", "recovered"] {
        assert!(
            Instant::now() < deadline,
            "told {:?}",
            app.world().resource::<Told>().0
        );
        frame(&mut app);
    }
    // The reconciler rebuilds a lane whose target unit was replaced (the
    // node's generation moved), from the same declarations.
    assert_ne!(lane_of(&app), lane, "the lane is rebuilt for the new unit");
    assert!(lane_of(&app).is_some());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(value(&app)) {
        assert!(
            Instant::now() < deadline,
            "the lane does not reach the replacement: {:?}",
            value(&app)
        );
        frame(&mut app);
    }
}

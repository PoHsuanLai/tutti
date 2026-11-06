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
use bevy_tutti::graph::{
    AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
};
use bevy_tutti::plugin_host::{
    PluginEmitter, PluginHealth, PluginLiveness, PluginLoadTerminated, PluginRecovered,
    PluginReloading, PluginRequest, TuttiHostingPlugin,
};
use tutti_core::AudioNode;
use tutti_core::transport::{ClickState, Transport};
use tutti_plugin::catalog::PluginId;
use tutti_types::SampleRate;

const SAMPLE_RATE: f64 = 48_000.0;

#[derive(Component)]
struct Mine;

/// What the entity events said, in order.
#[derive(Resource, Default)]
struct Told(Vec<&'static str>);

fn app(once: &str) -> App {
    // SAFETY: nextest runs this test in its own process, and nothing else in it
    // reads the environment concurrently.
    unsafe {
        std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server());
        std::env::set_var("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
        std::env::set_var("TUTTI_CLAP_PROBE_ONCE_FILE", once);
        std::env::set_var("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "3");
    }
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(MetronomeRes(std::sync::Arc::new(ClickState::new())));
    app.init_resource::<Told>();
    app.add_plugins((GraphReconcilePlugin, TuttiHostingPlugin));
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
    let mut app = app(&once.to_string_lossy());
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
                .map(|e| (e.handle.status(), e.handle.starved_blocks())),
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

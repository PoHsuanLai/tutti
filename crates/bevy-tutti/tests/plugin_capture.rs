//! A hosted plugin, loaded through the real request pipeline, is driven through
//! the shadow captured at load — never through the graph.
//!
//! `plugin_host::bind` and `plugin_host::latency` reach a plugin through its
//! [`PluginShadow`]: the node's `PluginControls`, taken from the unit in
//! `plugin_load_promote` before the unit moves into the graph. This file puts a
//! real plugin behind that path — the reference CLAP plugin, loaded by a real
//! `plugin-server` subprocess — and checks each consumer of the shadow did its
//! job:
//!
//! - the shadow is there, bound to the entity's node;
//! - the transport binding ran (`PluginTransportBound`), which it only does
//!   through the shadow;
//! - the latency poll recorded the plugin's declared latency
//!   (`CompensatedLatency`), which it reads only through the shadow — the first
//!   test in this crate to watch that poll see a live plugin, which
//!   `plugin_host::latency`'s module docs record as missing;
//! - the MIDI target was captured too, since `PluginClient` is registered with
//!   the MIDI registry by the hosting plugin.
//!
//! # Mutation
//!
//! Replacing `capture.capture(unit.as_ref())` in `plugin_load_promote` with
//! `CapturedControls::default()` fails with every observation empty: no MIDI
//! target, no shadow, so no transport bind and no latency record. Dropping only
//! the `plugin` arm of `CapturedControls::from_registries` (always `None`)
//! fails with the MIDI target still present and the other three empty, which
//! is how the test tells the two captures apart.
//!
//! # The two artifacts
//!
//! The plugin is a dev-dependency, so cargo builds its cdylib alongside this
//! test. The server is not (it would be a dependency cycle through
//! `tutti-plugin`), so it must be built first: `cargo build -p
//! tutti-plugin-server`. Both are looked up beside this test binary.

#![cfg(feature = "plugin")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bevy_app::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
};
use bevy_tutti::midi::MidiTarget;
use bevy_tutti::plugin_host::{
    CompensatedLatency, PluginLoadTerminated, PluginRequest, PluginShadow, PluginTransportBound,
    TuttiHostingPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::{ClickState, Transport};
use tutti_core::{AudioNode, SampleRate, Samples};
use tutti_plugin::catalog::PluginId;

const SAMPLE_RATE: f64 = 48_000.0;

/// `<target>/<profile>` — the directory this test binary's `deps/` sits in.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    exe.parent()
        .and_then(Path::parent)
        .expect("the test binary lives in <profile>/deps")
        .to_path_buf()
}

/// The newest of `<profile>/deps/<name>` and `<profile>/<name>` — cargo writes
/// the first and uplifts to the second without always refreshing it.
fn beside_this_binary(name: &str) -> Option<PathBuf> {
    let profile = profile_dir();
    let candidates = [
        profile
            .join("deps")
            .join(name)
            .to_string_lossy()
            .into_owned(),
        profile.join(name).to_string_lossy().into_owned(),
    ];
    tutti_fixture_resolve::newest_existing(candidates.iter().map(String::as_str)).map(PathBuf::from)
}

fn plugin_server() -> PathBuf {
    let name = if cfg!(windows) {
        "plugin-server.exe"
    } else {
        "plugin-server"
    };
    beside_this_binary(name).unwrap_or_else(|| {
        panic!(
            "`plugin-server` not found under {}. It is not a dev-dependency (that \
             would be a cycle through `tutti-plugin`), so build it first:\n\n  \
             cargo build -p tutti-plugin-server\n",
            profile_dir().display()
        )
    })
}

/// The reference CLAP cdylib, published under a `.clap` name — the server picks
/// the loader by extension, and reads a bare `.so` as VST2.
///
/// Published by atomic rename because `tutti-plugin`'s suites publish the same
/// link from their own processes; see their `clap_probe_path` for the race a
/// remove-then-link pair loses.
fn clap_probe() -> PathBuf {
    let lib = tutti_fixture_resolve::lib_filename("tutti_clap_test_plugin");
    let real = beside_this_binary(&lib).unwrap_or_else(|| {
        panic!(
            "the reference plugin `{lib}` is a dev-dependency built by this same \
             test run, but is not under {}",
            profile_dir().display()
        )
    });
    let link = real.with_extension("clap");
    let staging = link.with_extension(format!("clap.tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &staging).expect("stage the reference plugin symlink");
    #[cfg(windows)]
    std::fs::copy(&real, &staging).expect("stage the reference plugin copy");
    if std::fs::rename(&staging, &link).is_err() {
        let _ = std::fs::remove_file(&staging);
        assert!(
            link.exists(),
            "publish the reference plugin at {}",
            link.display()
        );
    }
    link
}

fn app() -> App {
    // SAFETY: nextest runs this test in its own process, and nothing else in it
    // reads the environment concurrently; the server path is the only lever
    // `Plugin::open_with` offers.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server()) };

    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    // Both are what `plugin_bind_transport` installs; it waits without them.
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
        transport_bound: world.get::<PluginTransportBound>(entity).is_some(),
        compensated_latency: world.get::<CompensatedLatency>(entity).map(|c| c.0),
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
            transport_bound: true,
            // Read off the shadow's latency cell: the plugin's own figure.
            compensated_latency: Some(Samples(
                tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize
            )),
        },
    );
}

/// What the capture's consumers left on the entity.
#[derive(Debug, PartialEq)]
struct Observed {
    midi_target_node: Option<tutti_core::dsp::NodeId>,
    shadow_node: Option<tutti_core::dsp::NodeId>,
    transport_bound: bool,
    compensated_latency: Option<Samples>,
}

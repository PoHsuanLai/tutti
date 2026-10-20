//! **In-process plugins open one per frame.** A VST2 plugin opens in
//! `plugin_load_promote`, on the frame thread (VST2 confines its load to the
//! main thread), and its open (the library's init, the state restore, the
//! parameter read) stalls the frame it lands in. Two loads landing together
//! open in two frames, not one.
//!
//! # Mutation
//!
//! - (run) `plugin_load_promote` without the cap (the `on_main &&
//!   opened_on_main` skip taken out) → both open in the same frame →
//!   fails.

#![cfg(feature = "vst2")]

#[macro_use]
mod common;

use common::plugin::vst2_probe;

use std::time::{Duration, Instant};

use bevy_app::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::plugin_host::{PluginLoadTerminated, PluginRequest, TuttiHostingPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::{AudioNode, SampleRate};
use tutti_plugin::catalog::PluginId;

const SAMPLE_RATE: f64 = 48_000.0;

#[test]
fn two_in_process_loads_landing_together_open_in_two_frames() {
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.add_plugins((GraphReconcilePlugin, TuttiHostingPlugin));

    let entities: Vec<_> = (0..2)
        .map(|_| {
            app.world_mut()
                .spawn(PluginRequest {
                    id: PluginId::from_path(vst2_probe()),
                    sample_rate: SampleRate(SAMPLE_RATE),
                    ..Default::default()
                })
                .id()
        })
        .collect();
    let opened = |app: &App| {
        entities
            .iter()
            .filter(|&&e| app.world().get::<PluginLoadTerminated>(e).is_some())
            .count()
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut frames_that_opened = 0;
    let mut before = 0;
    while before < entities.len() {
        assert!(Instant::now() < deadline, "the plugins never loaded");
        app.update();
        let now = opened(&app);
        assert!(
            now - before <= 1,
            "{} in-process plugins opened in one frame",
            now - before
        );
        if now > before {
            frames_that_opened += 1;
        }
        before = now;
        // Long enough for both jobs (which return at once for an
        // in-process plugin) to finish before the next frame polls them.
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(frames_that_opened, 2);
    for &e in &entities {
        assert!(
            app.world().get::<AudioNode>(e).is_some(),
            "each plugin is bound to a node"
        );
    }
}
